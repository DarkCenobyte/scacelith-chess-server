//! The lobby actor's state and handlers (a port of `control-plane.js`; see the module
//! documentation of [`super`]).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use scacelith_protocol::{
    self as proto, ChallengeReceived, ChallengeStatus, ErrorCode, NoticeCode, PlayerInfo, QueueStatus,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use super::refunds::RefundNotices;
use super::{ClaimOutcome, LobbyMsg, LobbyRequest, Timer};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::{GameEnded, IncidentKind, NewGame, RematchRequest, SanctionApplied};
use crate::ids::{self, GameId, UserId};
use crate::log::{self, Logger};
use crate::matching::challenges::{
    Challenge, ChallengeKind, ChallengePlayer, Challenges, CreateRequest, Started, TargetUser,
};
use crate::matching::conduct::{Conduct, Cooldown, IncidentOutcome, record_incident};
use crate::matching::elo::{CUSTOM_CATEGORY, Categories};
use crate::matching::matchmaker::{
    JoinRequest, Matchmaker, PAIR_RETRY_DELAY_MS, PairedPlayer, Pairing, QUEUE_REFRESH_MS,
};
use crate::matching::{ChallengeState, ColorPref, MatchError, QueueState};
use crate::net::limits::SharedLimits;
use crate::realtime::deps::GameHosts;
use crate::realtime::frames;
use crate::realtime::link::{ConnCmd, ConnLink};
use crate::realtime::metrics;
use crate::realtime::reads::{self, DbConduct};
use crate::store::{PendingRefund, PendingRefunds, Store, StoreError};
use crate::{log_error, log_security};

/// How long the lobby waits for a host to create a game (a game created later is cancelled).
pub(crate) const CREATE_TIMEOUT: Duration = Duration::from_secs(5);
/// Window of the per-player limits (CHALLENGE_UNPLAYED_PER_MIN, PRIVATE_CODE_FAILURES_PER_MIN).
const PLAYER_LIMIT_WINDOW_MS: u64 = 60_000;
/// Challenge expiry sweep.
const EXPIRE_EVERY: Duration = Duration::from_secs(1);
/// Ban cache and rate limit sweep.
const SWEEP_EVERY: Duration = Duration::from_secs(10);

/// What the lobby needs.
pub(crate) struct LobbyDeps {
    pub config: Arc<Config>,
    pub clock: SharedClock,
    pub store: Store,
    pub hosts: Arc<dyn GameHosts>,
    /// The process-wide sliding windows (swept by the lobby every 10 s).
    pub limits: Arc<SharedLimits>,
    pub matchmaker: Matchmaker,
    pub challenges: Challenges,
    /// How long a host may take to create a game ([`CREATE_TIMEOUT`]).
    pub create_timeout: Duration,
    pub log: Logger,
}

impl LobbyDeps {
    /// The lobby of `config`, with the matchmaker and challenges it configures.
    pub(crate) fn new(
        config: Arc<Config>,
        clock: SharedClock,
        store: Store,
        hosts: Arc<dyn GameHosts>,
        limits: Arc<SharedLimits>,
    ) -> LobbyDeps {
        LobbyDeps {
            matchmaker: Matchmaker::from_config(&config),
            challenges: Challenges::from_config(&config),
            config,
            clock,
            store,
            hosts,
            limits,
            create_timeout: CREATE_TIMEOUT,
            log: Logger::root().child("lobby"),
        }
    }
}

/// The results of the lobby's tasks.
pub(crate) enum Done {
    Created {
        token: u64,
        result: CreateResult,
    },
    RematchChecked {
        request: RematchRequest,
        reply: oneshot::Sender<Result<GameId, ErrorCode>>,
        bans: [Option<i64>; 2],
        cooldowns: [Option<Cooldown>; 2],
    },
    ConductRecorded {
        user: UserId,
        kind: IncidentKind,
        result: Result<IncidentOutcome, StoreError>,
    },
    RefundsPolled {
        result: Result<Vec<PendingRefund>, StoreError>,
    },
    RefundsRead {
        user: UserId,
        seen: u32,
        result: Result<PendingRefunds, StoreError>,
    },
    RefundsMarked {
        user: UserId,
        seen: u32,
        pending: PendingRefunds,
        result: Result<usize, StoreError>,
    },
}

/// How a game creation ended.
pub(crate) enum CreateResult {
    /// Players found banned in the database (with the end of their ban): no game.
    Banned(Vec<(UserId, i64)>),
    Failed(ErrorCode),
    Created(GameId),
}

/// What made a game (`scacelith_games_created_total{source}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Queue,
    Challenge,
    Rematch,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Queue => "queue",
            Source::Challenge => "challenge",
            Source::Rematch => "rematch",
        }
    }
}

/// What to do once a creation ended.
enum After {
    /// A queue pairing: on failure both players go back to the queue.
    Queue(Pairing),
    /// An accepted challenge or a joined private code: the creator is told, the request of the
    /// player who accepted is answered.
    Challenge { challenge: Challenge, link: Arc<ConnLink>, seq: u32 },
    /// A rematch: the host gets the new game id or the error.
    Rematch(oneshot::Sender<Result<GameId, ErrorCode>>),
}

/// One side of a game to create.
#[derive(Clone)]
struct Side {
    info: PlayerInfo,
    /// The rating is read again from the store (challenges and rematches).
    reread: bool,
}

/// A game to create (the lobby's half of [`NewGame`]).
#[derive(Clone)]
struct Order {
    category: String,
    base_ms: u32,
    inc_ms: u32,
    rated: bool,
    white: Side,
    black: Side,
    rematch_of: Option<GameId>,
    auto_press: bool,
}

struct Creation {
    white: UserId,
    black: UserId,
    rated: bool,
    source: Source,
    after: After,
}

#[derive(Clone, Debug)]
struct Queued {
    category: String,
    rated: bool,
}

/// The lobby actor.
pub(crate) struct LobbyActor {
    pub(super) config: Arc<Config>,
    pub(super) clock: SharedClock,
    pub(super) store: Store,
    hosts: Arc<dyn GameHosts>,
    limits: Arc<SharedLimits>,
    create_timeout: Duration,
    pub(super) log: Logger,
    conduct_log: Logger,
    categories: Categories,
    pub(super) presence: crate::realtime::presence::Presence,
    mm: Matchmaker,
    ch: Challenges,
    conduct: Conduct,
    /// User to game in progress.
    active_games: HashMap<UserId, GameId>,
    /// Users whose game is being created.
    starting: HashSet<UserId>,
    /// Users searching.
    queued: HashMap<UserId, Queued>,
    /// User to ban end (sanctions told to the lobby).
    bans: HashMap<UserId, i64>,
    creations: HashMap<u64, Creation>,
    next_token: u64,
    pub(super) refunds: RefundNotices,
    /// Our own inbox, for the results of our tasks.
    tx: mpsc::UnboundedSender<LobbyMsg>,
    pending_tasks: usize,
    pub(super) timers_on: bool,
}

fn rating_u16(r: i64) -> u16 {
    r.clamp(0, i64::from(u16::MAX)) as u16
}

fn u32_ms(v: i64) -> u32 {
    v.clamp(0, i64::from(u32::MAX)) as u32
}

fn match_error(e: MatchError) -> ErrorCode {
    ErrorCode::from_u8(e.code())
}

fn queue_state(s: QueueState) -> proto::QueueState {
    proto::QueueState::from_u8(s as u8)
}

fn color(c: ColorPref) -> proto::ColorPref {
    proto::ColorPref::from_u8(c as u8).unwrap_or(proto::ColorPref::Random)
}

fn status_frame(c: &Challenge, state: ChallengeState) -> Option<Bytes> {
    frames::encode(&ChallengeStatus {
        id: c.id,
        state: proto::ChallengeState::from_u8(state as u8),
        target: c.target.clone(),
        code: c.code.clone(),
        base_sec: u16::try_from(c.base_sec).unwrap_or(u16::MAX),
        inc_sec: u8::try_from(c.inc_sec).unwrap_or(u8::MAX),
        rated: c.rated,
    })
}

fn queue_frame(
    category: &str,
    rated: bool,
    state: QueueState,
    wait_ms: u32,
    window: u16,
    queued: u32,
) -> Option<Bytes> {
    frames::encode(&QueueStatus {
        category: category.to_string(),
        rated,
        state: queue_state(state),
        wait_ms,
        window,
        queued,
    })
}

fn player_info(user_id: UserId, name: &str, rating: i64, provisional: bool) -> PlayerInfo {
    PlayerInfo { user_id, name: name.to_string(), rating: rating_u16(rating), provisional }
}

impl LobbyActor {
    pub(super) fn new(deps: LobbyDeps, tx: mpsc::UnboundedSender<LobbyMsg>) -> LobbyActor {
        LobbyActor {
            categories: Categories::from_config(&deps.config),
            conduct_log: Logger::root().child("conduct"),
            config: deps.config,
            clock: deps.clock,
            store: deps.store,
            hosts: deps.hosts,
            limits: deps.limits,
            create_timeout: deps.create_timeout,
            log: deps.log,
            presence: Default::default(),
            mm: deps.matchmaker,
            ch: deps.challenges,
            conduct: Conduct::new(),
            active_games: HashMap::new(),
            starting: HashSet::new(),
            queued: HashMap::new(),
            bans: HashMap::new(),
            creations: HashMap::new(),
            next_token: 0,
            refunds: RefundNotices::default(),
            tx,
            pending_tasks: 0,
            timers_on: false,
        }
    }

    /// The actor's loop: timers first (they are cheap and must not starve), then the inbox.
    pub(super) async fn run(mut self, mut rx: mpsc::UnboundedReceiver<LobbyMsg>, timers: bool) {
        self.timers_on = timers;
        let every = |d: Duration| {
            let mut i = tokio::time::interval_at(Instant::now() + d, d);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        };
        let tick_ms = u64::try_from(self.config.match_tick_ms).unwrap_or(250).max(1);
        let mut tick = every(Duration::from_millis(tick_ms));
        let mut refresh = every(Duration::from_millis(QUEUE_REFRESH_MS as u64));
        let mut expire = every(EXPIRE_EVERY);
        let mut sweep = every(SWEEP_EVERY);
        let mut poll = every(super::refunds::REFUND_POLL);
        if timers {
            self.refunds_poll();
        }
        loop {
            let retry_at = self.next_refund_retry();
            tokio::select! {
                biased;
                _ = tick.tick(), if self.timers_on => self.on_timer(Timer::MatchTick),
                _ = refresh.tick(), if self.timers_on => self.on_timer(Timer::RefreshQueues),
                _ = expire.tick(), if self.timers_on => self.on_timer(Timer::ExpireChallenges),
                _ = sweep.tick(), if self.timers_on => self.on_timer(Timer::Sweep),
                _ = poll.tick(), if self.timers_on => self.on_timer(Timer::RefundPoll),
                _ = tokio::time::sleep_until(retry_at.unwrap_or_else(far_future)), if self.timers_on && retry_at.is_some() => {
                    self.on_timer(Timer::RefundRetries);
                }
                msg = rx.recv() => match msg {
                    None | Some(LobbyMsg::Stop) => break,
                    Some(msg) => self.handle(msg).await,
                },
            }
            self.update_gauges();
        }
    }

    fn update_gauges(&self) {
        let m = metrics::lobby();
        m.online.set(self.presence.len() as f64);
        m.searching.set(self.queued.len() as f64);
        m.challenges_open.set(self.ch.len() as f64);
    }

    pub(super) fn now(&self) -> i64 {
        self.clock.wall_ms()
    }

    async fn handle(&mut self, msg: LobbyMsg) {
        match msg {
            LobbyMsg::Claim { link, ban, reply } => self.claim(link, ban, reply),
            LobbyMsg::Release { user, conn } => self.release(user, conn),
            LobbyMsg::Request { link, seq, req } => self.request(link, seq, req).await,
            LobbyMsg::GameEnded(ended) => self.game_ended(&ended),
            LobbyMsg::GameRecovered { game, white, black } => self.game_recovered(game, white, black),
            LobbyMsg::Rematch { request, reply } => self.rematch(request, reply),
            LobbyMsg::Conduct { user, kind } => self.conduct_record(user, kind),
            LobbyMsg::SessionsRevoked { user, token_hashes } => {
                self.sessions_revoked(user, token_hashes.as_deref())
            }
            LobbyMsg::SanctionApplied(s) => self.sanction_applied(s),
            LobbyMsg::RefundsPending => self.refunds_poll(),
            LobbyMsg::Done(done) => {
                self.pending_tasks = self.pending_tasks.saturating_sub(1);
                self.done(done);
            }
            LobbyMsg::StopTimers => {
                self.timers_on = false;
                self.refunds_stop();
            }
            LobbyMsg::Ping(reply) => {
                let _ = reply.send(self.pending_tasks);
            }
            LobbyMsg::Stop => {}
            #[cfg(test)]
            LobbyMsg::Timer(t) => self.on_timer(t),
        }
    }

    pub(super) fn on_timer(&mut self, timer: Timer) {
        match timer {
            Timer::MatchTick => self.match_tick(),
            Timer::RefreshQueues => self.refresh_queues(),
            Timer::ExpireChallenges => self.expire_challenges(),
            Timer::Sweep => self.sweep(),
            Timer::RefundPoll => self.refunds_poll(),
            Timer::RefundRetries => self.refund_retries(),
        }
    }

    /// Runs a task whose result comes back as [`LobbyMsg::Done`].
    pub(super) fn spawn(&mut self, task: impl Future<Output = Done> + Send + 'static) {
        self.pending_tasks += 1;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let done = task.await;
            let _ = tx.send(LobbyMsg::Done(done));
        });
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Created { token, result } => self.created(token, result),
            Done::RematchChecked { request, reply, bans, cooldowns } => {
                self.rematch_checked(request, reply, bans, cooldowns);
            }
            Done::ConductRecorded { user, kind, result } => self.conduct_recorded(user, kind, result),
            Done::RefundsPolled { result } => self.refunds_polled(result),
            Done::RefundsRead { user, seen, result } => self.refunds_read(user, seen, result),
            Done::RefundsMarked { user, seen, pending, result } => {
                self.refunds_marked(user, seen, pending, result)
            }
        }
    }

    // ---- helpers --------------------------------------------------------------------------------

    /// Queues a frame for the user's live connection.
    pub(super) fn send_user(&self, user: UserId, frame: Option<Bytes>) -> bool {
        match (self.presence.get(user), frame) {
            (Some(link), Some(frame)) => link.send(frame),
            _ => false,
        }
    }

    fn kick(&self, user: UserId, reason: &str, code: u16, frames: &[Bytes]) {
        if let Some(link) = self.presence.get(user) {
            metrics::lobby().kicks.with(&[reason]).inc();
            link.kick(frames, code);
        }
    }

    /// Answers a lobby request (exactly one `Ack` or `Error` per request).
    fn answer(link: &ConnLink, seq: u32, result: Result<(), ErrorCode>) {
        link.send(match result {
            Ok(()) => frames::ack(seq),
            Err(code) => frames::error(seq, code, false, 0),
        });
        link.end_request();
    }

    /// End of the user's ban: the cached one, else the stored one read by the caller.
    fn ban_until(&self, user: UserId, now: i64, stored: Option<i64>) -> i64 {
        match self.bans.get(&user) {
            Some(&until) if until > now => until,
            _ => stored.unwrap_or(0),
        }
    }

    /// Whether the user is banned now. A ban found only in the database (given with the admin
    /// commands, which write nothing but the database) is enforced as a sanction is, without
    /// being cached: a later unban counts at once.
    fn banned(&mut self, user: UserId, now: i64, stored: Option<i64>) -> bool {
        let until = self.ban_until(user, now, stored);
        if until <= now {
            return false;
        }
        if self.bans.get(&user).is_none_or(|&cached| cached <= now) {
            self.enforce_ban(user, until, "stored ban");
        }
        true
    }

    pub(super) fn busy(&self, user: UserId) -> bool {
        self.active_games.contains_key(&user) || self.starting.contains(&user)
    }

    /// End of the user's rated matchmaking pause: the cached state, else the stored one read by
    /// the caller (then cached).
    fn cooldown_until(&mut self, user: UserId, now: i64, stored: Option<Cooldown>) -> i64 {
        if let Some(until) = self.conduct.cooldown_until(user, now) {
            return until;
        }
        match stored {
            Some(state) => {
                self.conduct.remember(user, state, now);
                state.active_until(now)
            }
            None => 0,
        }
    }

    fn username_of(&self, user: UserId, given: &str) -> String {
        if !given.is_empty() {
            return given.to_string();
        }
        self.presence.get(user).map(|l| l.username().to_string()).unwrap_or_default()
    }

    fn challenge_player(link: &ConnLink, rating: i64, provisional: bool) -> ChallengePlayer {
        ChallengePlayer {
            user_id: link.user_id(),
            username: link.username().to_string(),
            rating,
            provisional,
            conn_id: link.conn_id(),
        }
    }

    // ---- presence -------------------------------------------------------------------------------

    fn claim(&mut self, link: Arc<ConnLink>, stored_ban: Option<i64>, reply: oneshot::Sender<ClaimOutcome>) {
        let user = link.user_id();
        let conn = link.conn_id();
        let now = self.now();
        let until = self.ban_until(user, now, stored_ban);
        let outcome = if until > now {
            ClaimOutcome::Banned { until }
        } else {
            // The exact MAX_CONNECTIONS check (the upgrade allows a reserve beyond it). A player
            // with a game in progress is admitted anyway: refusing them would lose the game.
            let full = self.presence.len() as i64 >= self.config.max_connections;
            if full && self.presence.get(user).is_none() && !self.active_games.contains_key(&user) {
                ClaimOutcome::Full
            } else {
                if let Some(previous) = self.presence.claim(link) {
                    metrics::lobby().kicks.with(&["replaced"]).inc();
                    previous.kick(
                        &[
                            frames::notice(NoticeCode::ReplacedByNewConnection, 0.0),
                            frames::error(0, ErrorCode::Replaced, true, 0),
                        ],
                        proto::close_code_for(ErrorCode::Replaced).unwrap_or(4007),
                    );
                    self.leave_queue(user, false);
                }
                let active_game = self.active_games.get(&user).copied().unwrap_or(0);
                self.refunds_connected(user, active_game);
                ClaimOutcome::Admitted { active_game }
            }
        };
        let admitted = matches!(outcome, ClaimOutcome::Admitted { .. });
        if reply.send(outcome).is_err() && admitted {
            // The connection went away during the claim.
            self.release(user, conn);
        }
    }

    fn release(&mut self, user: UserId, conn: u32) {
        if self.presence.release(user, conn) {
            self.user_gone(user);
        }
    }

    fn user_gone(&mut self, user: UserId) {
        self.leave_queue(user, false);
        self.drop_challenges_of(user);
    }

    // ---- requests -------------------------------------------------------------------------------

    async fn request(&mut self, link: Arc<ConnLink>, seq: u32, req: LobbyRequest) {
        let user = link.user_id();
        match req {
            LobbyRequest::QueueJoin { category, rated, rating, provisional, ban, cooldown } => {
                let r = self.queue_join(&link, &category, rated, rating, provisional, ban, cooldown);
                Self::answer(&link, seq, r);
                if r.is_ok() {
                    self.send_queue_status(user);
                }
            }
            LobbyRequest::QueueLeave => {
                self.leave_queue(user, true);
                Self::answer(&link, seq, Ok(()));
            }
            LobbyRequest::ChallengeCreate {
                target,
                base_sec,
                inc_sec,
                rated,
                color,
                rating,
                provisional,
                ban,
            } => {
                let player = Self::challenge_player(&link, rating, provisional);
                let r = self.challenge_create(player, target, base_sec, inc_sec, rated, color, ban).await;
                Self::answer(&link, seq, r);
            }
            LobbyRequest::ChallengeAccept { id } => self.challenge_accept(link, seq, id),
            LobbyRequest::ChallengeJoinCode { code } => self.challenge_join_code(link, seq, &code),
            LobbyRequest::ChallengeDecline { id } => {
                let r = self.challenge_decline(user, id);
                Self::answer(&link, seq, r);
            }
            LobbyRequest::ChallengeCancel { id } => {
                let r = self.challenge_cancel(user, id);
                Self::answer(&link, seq, r);
            }
        }
    }

    // ---- matchmaking ----------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn queue_join(
        &mut self,
        link: &ConnLink,
        category: &str,
        rated: bool,
        rating: i64,
        provisional: bool,
        stored_ban: Option<i64>,
        stored_cooldown: Option<Cooldown>,
    ) -> Result<(), ErrorCode> {
        let user = link.user_id();
        let now = self.now();
        if self.presence.get(user).is_none_or(|cur| cur.conn_id() != link.conn_id()) {
            return Err(ErrorCode::QueueNotAllowed);
        }
        if self.banned(user, now, stored_ban) {
            return Err(ErrorCode::Banned);
        }
        if self.busy(user) {
            return Err(ErrorCode::AlreadyInGame);
        }
        if self.categories.parse(category).is_none() {
            return Err(ErrorCode::InvalidCategory);
        }
        if rated {
            let until = self.cooldown_until(user, now, stored_cooldown);
            if until > now {
                self.send_user(user, Some(frames::notice(NoticeCode::MatchmakingCooldown, until as f64)));
                return Err(ErrorCode::MatchmakingCooldown);
            }
        }
        if self.queued.contains_key(&user) || self.mm.has(user) {
            self.mm.leave(user);
        }
        self.mm
            .join(JoinRequest {
                user_id: user,
                username: link.username().to_string(),
                category: category.to_string(),
                rated,
                rating,
                provisional,
                conn_id: link.conn_id(),
                color_balance: None,
                joined_at: now,
                recent_opponents: Vec::new(),
            })
            .map_err(match_error)?;
        self.queued.insert(user, Queued { category: category.to_string(), rated });
        Ok(())
    }

    /// Takes the user out of the queue; with `notify`, a searching user gets `QueueStatus{Left}`.
    fn leave_queue(&mut self, user: UserId, notify: bool) {
        let q = self.queued.remove(&user);
        let was = self.mm.leave(user);
        if notify && (q.is_some() || was) {
            let (category, rated) = q.map_or((String::new(), false), |q| (q.category, q.rated));
            self.send_user(user, queue_frame(&category, rated, QueueState::Left, 0, 0, 0));
        }
    }

    /// Sends the current `QueueStatus` to a searching player.
    fn send_queue_status(&self, user: UserId) {
        if !self.queued.contains_key(&user) {
            return;
        }
        if let Some(st) = self.mm.status_of(user, self.now()) {
            self.send_user(
                user,
                queue_frame(&st.category, st.rated, st.state, st.wait_ms, st.window, st.queued),
            );
        }
    }

    fn refresh_queues(&self) {
        for (user, st) in self.mm.statuses(self.now()) {
            if self.queued.contains_key(&user) {
                self.send_user(
                    user,
                    queue_frame(&st.category, st.rated, st.state, st.wait_ms, st.window, st.queued),
                );
            }
        }
    }

    fn match_tick(&mut self) {
        let now = self.now();
        for pairing in self.mm.tick(now) {
            self.start_pairing(pairing, now);
        }
    }

    fn start_pairing(&mut self, pairing: Pairing, now: i64) {
        for e in [&pairing.white, &pairing.black] {
            self.queued.remove(&e.user_id);
        }
        let Some(cat) = self.categories.parse(&pairing.category).cloned() else { return };
        for e in [&pairing.white, &pairing.black] {
            let wait = u32_ms(now - e.joined_at);
            self.send_user(
                e.user_id,
                queue_frame(&pairing.category, pairing.rated, QueueState::Matched, wait, 0, 0),
            );
        }
        let side = |e: &PairedPlayer| Side {
            info: player_info(e.user_id, &self.username_of(e.user_id, &e.username), e.rating, e.provisional),
            reread: false,
        };
        let order = Order {
            category: pairing.category.to_string(),
            base_ms: u32_ms(cat.base_ms),
            inc_ms: u32_ms(cat.inc_ms),
            rated: pairing.rated,
            white: side(&pairing.white),
            black: side(&pairing.black),
            rematch_of: None,
            auto_press: self.config.auto_press_clock,
        };
        self.create_game(order, None, Source::Queue, After::Queue(pairing));
    }

    /// A pairing whose game could not be created: its colours are given back, the pair is held
    /// for PAIR_RETRY_DELAY_MS, and each player still connected, idle and not banned goes back
    /// to the queue with the original waiting time (the others get `QueueStatus{Left}`).
    fn pairing_failed(&mut self, pairing: Pairing, banned: &[UserId]) {
        let now = self.now();
        let (white, black) = (&pairing.white, &pairing.black);
        self.mm.record_colors(black.user_id, white.user_id);
        self.mm.hold_pair(white.user_id, black.user_id, now + PAIR_RETRY_DELAY_MS);
        for e in [white, black] {
            let stale = self.presence.get(e.user_id).is_none_or(|p| p.conn_id() != e.conn_id);
            let banned = banned.contains(&e.user_id) || self.ban_until(e.user_id, now, None) > now;
            if stale || self.busy(e.user_id) || banned {
                self.send_user(
                    e.user_id,
                    queue_frame(&pairing.category, pairing.rated, QueueState::Left, 0, 0, 0),
                );
                continue;
            }
            if self.mm.join(e.rejoin_request()).is_ok() {
                self.queued.insert(
                    e.user_id,
                    Queued { category: pairing.category.to_string(), rated: pairing.rated },
                );
            }
        }
    }

    // ---- challenges -----------------------------------------------------------------------------

    /// A direct challenge withdrawn or declined counts toward its creator's
    /// CHALLENGE_UNPLAYED_PER_MIN: create/cancel cycles cannot flood a target with popups.
    fn unplayed(&self, c: &Challenge) {
        let limit = self.config.challenge_unplayed_per_min as f64;
        self.limits.take(&format!("challenge:u{}", c.from.user_id), limit, PLAYER_LIMIT_WINDOW_MS, 1.0);
    }

    #[allow(clippy::too_many_arguments)]
    async fn challenge_create(
        &mut self,
        from: ChallengePlayer,
        target: String,
        base_sec: u16,
        inc_sec: u8,
        rated: bool,
        color_pref: ColorPref,
        stored_ban: Option<i64>,
    ) -> Result<(), ErrorCode> {
        let now = self.now();
        if self.banned(from.user_id, now, stored_ban) {
            return Err(ErrorCode::Banned);
        }
        let mut target_user = None;
        if !target.is_empty() {
            let key = format!("challenge:u{}", from.user_id);
            if self.limits.peek(&key) + 1.0 > self.config.challenge_unplayed_per_min as f64 {
                return Err(ErrorCode::ChallengeLimit);
            }
            if let Some(tid) = self.presence.user_id_by_name(&target) {
                let accepts = match self.store.users().by_id(tid).await {
                    Ok(user) => user.is_none_or(|u| u.accept_challenges),
                    Err(e) => {
                        log_error!(self.log, "preference read failed", { "err": log::error(&e) });
                        true
                    }
                };
                // The presence may have changed meanwhile only through this actor: it did not.
                let username = self.presence.get(tid).map(|l| l.username().to_string()).unwrap_or_default();
                target_user =
                    Some(TargetUser { user_id: tid, username, accept_challenges: accepts, online: true });
                // Past MATCH_REPEAT_LIMIT, for a time control the challenge takes as rated: a wrong
                // one keeps its own error, and a target who refuses challenges keeps
                // UserUnavailable (nothing leaks: an offline target answers the same).
                let official =
                    self.categories.category_of(i64::from(base_sec) * 1000, i64::from(inc_sec) * 1000)
                        != CUSTOM_CATEGORY;
                if rated && accepts && official && self.mm.repeat_limited(from.user_id, tid, now) {
                    return Err(ErrorCode::RatedRepeatLimit);
                }
            }
        }
        let c = self
            .ch
            .create(
                CreateRequest {
                    from,
                    target,
                    target_user,
                    base_sec: i64::from(base_sec),
                    inc_sec: i64::from(inc_sec),
                    rated,
                    color: color_pref,
                },
                now,
            )
            .map_err(match_error)?;
        self.send_user(c.from.user_id, status_frame(&c, ChallengeState::Pending));
        if c.target_user_id != 0 {
            let received = ChallengeReceived {
                id: c.id,
                from: player_info(c.from.user_id, &c.from.username, c.from.rating, c.from.provisional),
                base_sec: u16::try_from(c.base_sec).unwrap_or(u16::MAX),
                inc_sec: u8::try_from(c.inc_sec).unwrap_or(u8::MAX),
                rated: c.rated,
                your_color: color(c.receiver_color),
                expires_ms: u32_ms(c.expires_at - now),
            };
            self.send_user(c.target_user_id, frames::encode(&received));
        }
        Ok(())
    }

    fn challenge_accept(&mut self, link: Arc<ConnLink>, seq: u32, id: u32) {
        let by = link.user_id();
        let now = self.now();
        if self.busy(by) {
            return Self::answer(&link, seq, Err(ErrorCode::AlreadyInGame));
        }
        // A busy creator is told to the challenge's target only: anyone else gets
        // ChallengeNotFound (nothing leaks).
        let creator_busy = self
            .ch
            .get(id, now)
            .filter(|c| c.kind == ChallengeKind::Direct && c.target_user_id == by)
            .is_some_and(|c| self.busy(c.from.user_id));
        if creator_busy {
            return Self::answer(&link, seq, Err(ErrorCode::AlreadyInGame));
        }
        match self.ch.accept(id, Self::challenge_player(&link, 0, false), now) {
            Ok(started) => self.start_challenge_game(started, link, seq),
            Err(e) => Self::answer(&link, seq, Err(match_error(e))),
        }
    }

    fn challenge_join_code(&mut self, link: Arc<ConnLink>, seq: u32, code: &str) {
        let by = link.user_id();
        if self.busy(by) {
            return Self::answer(&link, seq, Err(ErrorCode::AlreadyInGame));
        }
        let key = format!("joincode:u{by}");
        let limit = self.config.private_code_failures_per_min as f64;
        if self.limits.peek(&key) + 1.0 > limit {
            return Self::answer(&link, seq, Err(ErrorCode::RateLimited));
        }
        let now = self.now();
        // Past MATCH_REPEAT_LIMIT: refused before the code is used, so the creator's private game
        // stays pending.
        let creator = self.ch.get_code(code, now).filter(|c| c.rated).map(|c| c.from.user_id);
        if let Some(creator) = creator
            && self.mm.repeat_limited(creator, by, now)
        {
            return Self::answer(&link, seq, Err(ErrorCode::RatedRepeatLimit));
        }
        match self.ch.join_code(code, Self::challenge_player(&link, 0, false), now) {
            Ok(started) => self.start_challenge_game(started, link, seq),
            Err(e) => {
                if e == MatchError::CodeInvalid {
                    self.limits.take(&key, limit, PLAYER_LIMIT_WINDOW_MS, 1.0);
                }
                Self::answer(&link, seq, Err(match_error(e)));
            }
        }
    }

    fn start_challenge_game(&mut self, started: Started, link: Arc<ConnLink>, seq: u32) {
        let Started { challenge: c, game } = started;
        let now = self.now();
        let creator = c.from.user_id;
        if self.busy(creator) {
            self.send_user(creator, status_frame(&c, ChallengeState::Unavailable));
            return Self::answer(&link, seq, Err(ErrorCode::UserUnavailable));
        }
        let category = if game.category.is_empty() { c.category.clone() } else { game.category.clone() };
        let rated = game.rated && category != CUSTOM_CATEGORY;
        let initial = self.config.initial_rating;
        let side = |p: &ChallengePlayer| Side {
            info: player_info(p.user_id, &self.username_of(p.user_id, &p.username), initial, true),
            reread: true,
        };
        let (white, black) = (side(&game.white), side(&game.black));
        if rated && self.mm.repeat_limited(white.info.user_id, black.info.user_id, now) {
            self.send_user(creator, status_frame(&c, ChallengeState::Unavailable));
            return Self::answer(&link, seq, Err(ErrorCode::RatedRepeatLimit));
        }
        let order = Order {
            category,
            base_ms: u32_ms(game.base_ms),
            inc_ms: u32_ms(game.inc_ms),
            rated,
            white,
            black,
            rematch_of: None,
            auto_press: self.config.auto_press_clock,
        };
        self.create_game(order, None, Source::Challenge, After::Challenge { challenge: c, link, seq });
    }

    fn challenge_decline(&mut self, user: UserId, id: u32) -> Result<(), ErrorCode> {
        let c = self.ch.decline(id, user, self.now()).map_err(match_error)?;
        self.unplayed(&c);
        self.send_user(c.from.user_id, status_frame(&c, ChallengeState::Declined));
        Ok(())
    }

    fn challenge_cancel(&mut self, user: UserId, id: u32) -> Result<(), ErrorCode> {
        let c = self.ch.cancel(id, user, self.now()).map_err(match_error)?;
        if c.target_user_id != 0 {
            self.unplayed(&c);
            self.send_user(c.target_user_id, status_frame(&c, ChallengeState::Cancelled));
        }
        Ok(())
    }

    /// Expires challenges and private codes, and tells both sides.
    fn expire_challenges(&mut self) {
        for c in self.ch.expire(self.now()) {
            let frame = status_frame(&c, ChallengeState::Expired);
            self.send_user(c.from.user_id, frame.clone());
            if c.target_user_id != 0 {
                self.send_user(c.target_user_id, frame);
            }
        }
    }

    /// The user went offline: outgoing challenges are cancelled (their targets are told),
    /// incoming ones become Unavailable (their creators are told).
    fn drop_challenges_of(&mut self, user: UserId) {
        for c in self.ch.drop_user(user) {
            if c.from.user_id == user {
                if c.target_user_id != 0 {
                    self.send_user(c.target_user_id, status_frame(&c, ChallengeState::Cancelled));
                }
            } else {
                self.send_user(c.from.user_id, status_frame(&c, ChallengeState::Unavailable));
            }
        }
    }

    // ---- games ----------------------------------------------------------------------------------

    /// Creates a game on a host and attaches both players' live connections. The players count
    /// as busy from now on; the stored bans and the ratings to read again are looked up in the
    /// creation task, and the result comes back as [`Done::Created`].
    fn create_game(&mut self, order: Order, preferred: Option<u32>, source: Source, after: After) {
        let (white, black) = (order.white.info.user_id, order.black.info.user_id);
        let creation = Creation { white, black, rated: order.rated, source, after };
        if self.busy(white) || self.busy(black) {
            return self.finish_creation(creation, Err(ErrorCode::AlreadyInGame), &[]);
        }
        let now = self.now();
        if [white, black].iter().any(|u| self.bans.get(u).is_some_and(|&until| until > now)) {
            return self.finish_creation(creation, Err(ErrorCode::UserUnavailable), &[]);
        }
        self.starting.insert(white);
        self.starting.insert(black);
        let token = self.next_token;
        self.next_token += 1;
        self.creations.insert(token, creation);
        let task = CreateTask {
            store: self.store.clone(),
            hosts: self.hosts.clone(),
            clock: self.clock.clone(),
            config: self.config.clone(),
            timeout: self.create_timeout,
            log: self.log.clone(),
        };
        self.spawn(async move { Done::Created { token, result: task.run(order, preferred).await } });
    }

    fn created(&mut self, token: u64, result: CreateResult) {
        let Some(creation) = self.creations.remove(&token) else { return };
        self.starting.remove(&creation.white);
        self.starting.remove(&creation.black);
        let now = self.now();
        match result {
            CreateResult::Banned(banned) => {
                let users: Vec<UserId> = banned.iter().map(|&(u, _)| u).collect();
                for (user, until) in banned {
                    if self.bans.get(&user).is_none_or(|&cached| cached <= now) {
                        self.enforce_ban(user, until, "stored ban");
                    }
                }
                self.finish_creation(creation, Err(ErrorCode::UserUnavailable), &users);
            }
            CreateResult::Failed(code) => {
                metrics::lobby().create_failed.inc();
                self.finish_creation(creation, Err(code), &[]);
            }
            CreateResult::Created(game) => {
                metrics::lobby().created.with(&[creation.source.as_str()]).inc();
                if creation.rated {
                    self.mm.record_pairing(creation.white, creation.black, now);
                }
                for user in [creation.white, creation.black] {
                    self.active_games.insert(user, game);
                    self.leave_queue(user, creation.source != Source::Queue);
                    if let Some(link) = self.presence.get(user) {
                        link.command(ConnCmd::Attach(game));
                    }
                }
                self.finish_creation(creation, Ok(game), &[]);
            }
        }
    }

    fn finish_creation(&mut self, creation: Creation, result: Result<GameId, ErrorCode>, banned: &[UserId]) {
        match creation.after {
            After::Queue(pairing) => {
                if result.is_err() {
                    self.pairing_failed(pairing, banned);
                }
            }
            After::Challenge { challenge, link, seq } => {
                let creator = challenge.from.user_id;
                let state =
                    if result.is_ok() { ChallengeState::Accepted } else { ChallengeState::Unavailable };
                self.send_user(creator, status_frame(&challenge, state));
                Self::answer(&link, seq, result.map(|_| ()));
            }
            After::Rematch(reply) => {
                let _ = reply.send(result.map_err(|code| {
                    if code == ErrorCode::AlreadyInGame { ErrorCode::RematchUnavailable } else { code }
                }));
            }
        }
    }

    fn game_ended(&mut self, ended: &GameEnded) {
        for user in [ended.white, ended.black] {
            if self.active_games.get(&user) == Some(&ended.game) {
                self.active_games.remove(&user);
            }
        }
        self.refunds_game_ended(&[ended.white, ended.black]);
    }

    fn game_recovered(&mut self, game: GameId, white: UserId, black: UserId) {
        if !ids::is_game_id(game) {
            return;
        }
        for user in [white, black] {
            if user != 0 {
                self.active_games.entry(user).or_insert(game);
            }
        }
    }

    /// Both players asked for a rematch: their stored bans and cooldowns are read first.
    fn rematch(&mut self, request: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>) {
        let store = self.store.clone();
        let log = self.log.clone();
        let now = self.now();
        self.spawn(async move {
            let users = [request.game.white.user_id, request.game.black.user_id];
            let mut bans = [None; 2];
            let mut cooldowns = [None; 2];
            for (i, &user) in users.iter().enumerate() {
                bans[i] = reads::stored_ban(&store, user, now, &log).await;
                if request.game.rated {
                    cooldowns[i] = reads::stored_cooldown(&store, user, &log).await;
                }
            }
            Done::RematchChecked { request, reply, bans, cooldowns }
        });
    }

    fn rematch_checked(
        &mut self,
        request: RematchRequest,
        reply: oneshot::Sender<Result<GameId, ErrorCode>>,
        bans: [Option<i64>; 2],
        cooldowns: [Option<Cooldown>; 2],
    ) {
        let g = request.game;
        let old = g.rematch_of.unwrap_or(0);
        let now = self.now();
        let users = [g.white.user_id, g.black.user_id];
        for (i, &user) in users.iter().enumerate() {
            let unavailable = self.banned(user, now, bans[i])
                || self.presence.get(user).is_none()
                || self.active_games.get(&user).is_some_and(|&a| a != old)
                || self.starting.contains(&user)
                || (g.rated && self.cooldown_until(user, now, cooldowns[i]) > now);
            if unavailable {
                let _ = reply.send(Err(ErrorCode::RematchUnavailable));
                return;
            }
        }
        if g.rated && self.mm.repeat_limited(users[0], users[1], now) {
            let _ = reply.send(Err(ErrorCode::RematchUnavailable));
            return;
        }
        for user in users {
            if self.active_games.get(&user) == Some(&old) {
                self.active_games.remove(&user);
            }
        }
        let category = if g.category.is_empty() {
            self.categories.category_of(i64::from(g.base_ms), i64::from(g.inc_ms)).to_string()
        } else {
            g.category.clone()
        };
        let rated = g.rated && category != CUSTOM_CATEGORY;
        let initial = self.config.initial_rating;
        let side = |p: &PlayerInfo| Side {
            info: player_info(p.user_id, &self.username_of(p.user_id, &p.name), initial, true),
            reread: true,
        };
        let order = Order {
            category,
            base_ms: g.base_ms,
            inc_ms: g.inc_ms,
            rated,
            white: side(&g.white),
            black: side(&g.black),
            rematch_of: Some(old),
            auto_press: g.auto_press,
        };
        let preferred = ids::is_game_id(old).then(|| ids::shard_of(old));
        self.create_game(order, preferred, Source::Rematch, After::Rematch(reply));
    }

    /// A conduct incident: recorded on one write transaction, then cached.
    fn conduct_record(&mut self, user: UserId, kind: IncidentKind) {
        let store = self.store.clone();
        let limit = self.config.conduct_abandon_limit;
        let now = self.now();
        self.spawn(async move {
            let result = store
                .write(move |db| record_incident(&mut DbConduct { db, now }, user, kind, limit, now))
                .await;
            Done::ConductRecorded { user, kind, result }
        });
    }

    fn conduct_recorded(
        &mut self,
        user: UserId,
        kind: IncidentKind,
        result: Result<IncidentOutcome, StoreError>,
    ) {
        let now = self.now();
        match result {
            Ok(outcome) => {
                self.conduct.remember(user, outcome.state, now);
                if outcome.started {
                    log_security!(self.conduct_log, "conduct_cooldown", {
                        "userId": user, "kind": kind.as_str(), "incidents": outcome.incidents,
                        "cooldownLevel": outcome.level, "until": outcome.until,
                    });
                }
                if outcome.until > now {
                    self.send_user(
                        user,
                        Some(frames::notice(NoticeCode::MatchmakingCooldown, outcome.until as f64)),
                    );
                }
            }
            Err(e) => log_error!(self.log, "conduct.record failed", { "err": log::error(&e) }),
        }
    }

    // ---- sanctions and sessions -----------------------------------------------------------------

    fn sanction_applied(&mut self, s: SanctionApplied) {
        let end = if s.until > 0 { s.until } else { self.now() + reads::PERMANENT_BAN_HOLD_MS };
        self.bans.insert(s.user, end);
        self.enforce_ban(s.user, end, &s.reason);
        if s.refunds > 0 {
            self.refunds_poll();
        }
    }

    /// A banned player is kicked, forfeits their running game and leaves the queue and their
    /// challenges.
    fn enforce_ban(&mut self, user: UserId, end: i64, reason: &str) {
        log_security!(self.log, "sanction applied", { "userId": user, "until": end, "reason": reason });
        self.kick(
            user,
            "banned",
            proto::close_code_for(ErrorCode::Banned).unwrap_or(4004),
            &[frames::notice(NoticeCode::Banned, end as f64), frames::error(0, ErrorCode::Banned, true, 0)],
        );
        if let Some(&game) = self.active_games.get(&user) {
            self.hosts.forfeit_user(game, user);
        }
        self.user_gone(user);
    }

    /// Connections authenticated with a revoked token get `Notice{SessionRevoked}` and a fatal
    /// `Error{Unauthorized}` (no list: every session of the user was revoked).
    fn sessions_revoked(&mut self, user: UserId, token_hashes: Option<&[[u8; 32]]>) {
        let Some(link) = self.presence.get(user) else { return };
        if token_hashes.is_none_or(|hashes| hashes.contains(link.token_hash())) {
            link.kick(
                &[
                    frames::notice(NoticeCode::SessionRevoked, 0.0),
                    frames::error(0, ErrorCode::Unauthorized, true, 0),
                ],
                proto::close_code_for(ErrorCode::Unauthorized).unwrap_or(4003),
            );
        }
    }

    fn sweep(&mut self) {
        let now = self.now();
        self.limits.sweep();
        self.bans.retain(|_, until| *until > now);
    }
}

/// A far deadline for a disabled timer branch.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}

/// The work of a game creation, off the actor.
struct CreateTask {
    store: Store,
    hosts: Arc<dyn GameHosts>,
    clock: SharedClock,
    config: Arc<Config>,
    timeout: Duration,
    log: Logger,
}

impl CreateTask {
    async fn run(self, mut order: Order, preferred: Option<u32>) -> CreateResult {
        let now = self.clock.wall_ms();
        let mut banned = Vec::new();
        for user in [order.white.info.user_id, order.black.info.user_id] {
            if let Some(until) = reads::stored_ban(&self.store, user, now, &self.log).await
                && until > now
            {
                banned.push((user, until));
            }
        }
        if !banned.is_empty() {
            return CreateResult::Banned(banned);
        }
        for side in [&mut order.white, &mut order.black] {
            if side.reread {
                let (rating, provisional) = reads::rating_of(
                    &self.store,
                    &self.config,
                    side.info.user_id,
                    &order.category,
                    &self.log,
                )
                .await;
                side.info.rating = rating_u16(rating);
                side.info.provisional = provisional;
            }
        }
        let game = NewGame {
            category: order.category,
            base_ms: order.base_ms,
            inc_ms: order.inc_ms,
            rated: order.rated,
            white: order.white.info,
            black: order.black.info,
            created_at: now,
            rematch_of: order.rematch_of,
            auto_press: order.auto_press,
        };
        let mut create = self.hosts.create(preferred, game);
        tokio::select! {
            result = &mut create => match result {
                Ok(game) => CreateResult::Created(game),
                Err(code) => CreateResult::Failed(code),
            },
            () = tokio::time::sleep(self.timeout) => {
                log_error!(self.log, "game.create failed", { "err": "timeout" });
                // A game created after the timeout would wait for players nobody attaches: the
                // host cancels it.
                let hosts = self.hosts;
                tokio::spawn(async move {
                    if let Ok(game) = create.await {
                        hosts.cancel(game);
                    }
                });
                CreateResult::Failed(ErrorCode::Internal)
            }
        }
    }
}
