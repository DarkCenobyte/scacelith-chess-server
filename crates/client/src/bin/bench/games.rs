//! Scenario (c): live games, and the pair of bots it is made of (the REST scenario plays its
//! set-up games with the same pairs).
//!
//! Each game is one task owning both players' connections: one challenges the other, both play
//! uniformly random legal moves at the configured pace (the side to move thinks
//! `--move-interval-ms` +- `--jitter`), and both send head gestures at `--gesture-hz`. At
//! `--max-plies` the side to move resigns; after `--between-games-ms` the pair starts again.
//!
//! Because both ends of a game live in one task, every latency is measured on one clock with the
//! instant each message was read from its socket:
//! * `move_relay`: Move sent by the mover to the MoveMade read by the opponent;
//! * `move_confirm`: Move sent to the MoveMade read by the mover itself;
//! * `gesture_relay`: Gesture sent to the opponent's relayed Gesture (its yaw carries a sequence
//!   number);
//! * `game_start`: challenge sent to both players holding the GameSnapshot.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use scacelith_client::ClientError;
use scacelith_client::bot::{Applied, GameTracker, Rng};
use scacelith_protocol::Color;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{Interval, MissedTickBehavior};

use crate::conn::{Cmd, Conn, Event};
use crate::ctx::{Account, Ctx, measure, progress};
use crate::report::Step;
use crate::stats::Stats;

/// Gestures remembered per player for the relay latency: the yaw is `index - RING / 2`.
const RING: usize = 256;
/// Deadline of a game start (challenge to both snapshots).
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline of a game's `GameEnd` after a resignation.
const END_TIMEOUT: Duration = Duration::from_secs(5);

/// How a pair plays.
#[derive(Clone, Copy, Debug)]
pub struct PlayConfig {
    /// Think time per move.
    pub move_interval: Duration,
    /// Think time spread (0..1).
    pub jitter: f64,
    /// Time between two gestures of a player (`None`: no gestures).
    pub gesture_period: Option<Duration>,
    /// Resign at this ply.
    pub max_plies: u16,
    /// Time control: base seconds, increment seconds.
    pub tc: (u16, u8),
    /// Rated games.
    pub rated: bool,
}

struct Player {
    conn: Conn,
    tracker: Option<GameTracker>,
    ended: bool,
    gesture_seq: usize,
    sent: Vec<Option<Instant>>,
}

impl Player {
    fn new(conn: Conn) -> Player {
        Player { conn, tracker: None, ended: false, gesture_seq: 0, sent: vec![None; RING] }
    }
}

enum Wake {
    Msg(usize, Result<(Event, Instant), ClientError>),
    MoveDue,
    GestureTick,
    Stop,
}

/// How a game ended for the pair.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both players saw the `GameEnd`.
    Finished,
    /// The run is stopping.
    Stopped,
    /// A connection was lost (counted as `drop.<close code>`).
    Lost,
    /// The game did not start (counted as `error.<reason>`).
    NotStarted,
}

/// Two connected bots playing each other.
pub struct Pair {
    players: [Player; 2],
    names: [String; 2],
    game: u64,
    pending: Option<(u16, Instant)>,
    next_move: Option<(usize, tokio::time::Instant, u32)>,
    rng: Rng,
    stats: Arc<Stats>,
    games_played: usize,
    /// Games left running by an earlier run, resigned at the connection.
    leftover: [u64; 2],
}

async fn sleep_until_or_never(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn tick_or_never(interval: Option<&mut Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn color_of_ply(ply: u16) -> Color {
    if ply.is_multiple_of(2) { Color::White } else { Color::Black }
}

impl Pair {
    /// Connects both accounts (within the context's handshake pool); `None` when either fails.
    pub async fn connect(ctx: &Ctx, a: &Account, b: &Account, stats: Arc<Stats>, rng: Rng) -> Option<Pair> {
        let (ca, cb) = tokio::join!(ctx.connect(&a.token, &stats), ctx.connect(&b.token, &stats));
        let (ca, cb) = match (ca, cb) {
            (Some(ca), Some(cb)) => (ca, cb),
            (ca, cb) => {
                for c in [ca, cb].into_iter().flatten() {
                    c.close();
                }
                return None;
            }
        };
        // A game left running by an earlier run would make the challenges fail: resign it.
        let leftover = [ca.welcome().active_game, cb.welcome().active_game];
        for (c, &active) in [&ca, &cb].into_iter().zip(&leftover) {
            if active != 0 {
                let _ = c.send(Cmd::Resign { game: active });
                stats.add("leftover_games_resigned", 1);
            }
        }
        let names = [ca.welcome().username.clone(), cb.welcome().username.clone()];
        Some(Pair {
            players: [Player::new(ca), Player::new(cb)],
            names,
            game: 0,
            pending: None,
            next_move: None,
            rng,
            stats,
            games_played: 0,
            leftover,
        })
    }

    /// The id of the current (or last) game.
    pub fn game(&self) -> u64 {
        self.game
    }

    async fn wake(&mut self, gestures: Option<&mut Interval>, stop: &mut watch::Receiver<bool>) -> Wake {
        let due = self.next_move.map(|(_, at, _)| at);
        let [p0, p1] = &mut self.players;
        tokio::select! {
            r = p0.conn.recv() => Wake::Msg(0, r),
            r = p1.conn.recv() => Wake::Msg(1, r),
            () = sleep_until_or_never(due) => Wake::MoveDue,
            () = tick_or_never(gestures) => Wake::GestureTick,
            _ = stop.changed() => Wake::Stop,
        }
    }

    fn lost(&self, who: usize) -> Outcome {
        let code = self.players[who].conn.close_code().unwrap_or(1006);
        self.stats.add(&format!("drop.{code}"), 1);
        Outcome::Lost
    }

    /// Challenges and accepts a new game (the challenger alternates); `Err` with the reason it
    /// did not start.
    pub async fn start(&mut self, cfg: &PlayConfig, stop: &mut watch::Receiver<bool>) -> Result<(), Outcome> {
        let challenger = self.games_played % 2;
        let other = 1 - challenger;
        for p in &mut self.players {
            p.tracker = None;
            p.ended = false;
        }
        self.pending = None;
        self.next_move = None;
        self.game = 0;
        let sent = Instant::now();
        let seq = match self.players[challenger].conn.send(Cmd::Challenge {
            target: &self.names[other],
            base_sec: cfg.tc.0,
            inc_sec: cfg.tc.1,
            rated: cfg.rated,
        }) {
            Ok(seq) => seq,
            Err(_) => return Err(self.lost(challenger)),
        };
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            let wake = tokio::select! {
                w = self.wake(None, stop) => w,
                () = tokio::time::sleep_until(deadline) => {
                    self.stats.add("error.start_timeout", 1);
                    return Err(Outcome::NotStarted);
                }
            };
            let (who, event) = match wake {
                Wake::Stop => return Err(Outcome::Stopped),
                Wake::Msg(who, Ok((event, _))) => (who, event),
                Wake::Msg(who, Err(_)) => return Err(self.lost(who)),
                Wake::MoveDue | Wake::GestureTick => continue,
            };
            match event {
                Event::ChallengeReceived { id } if who == other => {
                    if self.players[other].conn.send(Cmd::Accept { id }).is_err() {
                        return Err(self.lost(other));
                    }
                }
                Event::Error { r#ref, code } if who == challenger && r#ref == seq => {
                    self.stats.add(&format!("error.challenge_{code}"), 1);
                    return Err(Outcome::NotStarted);
                }
                Event::Error { code, .. } => self.stats.add(&format!("error.server_{code}"), 1),
                Event::Snapshot(s)
                    if !s.over
                        && !self.leftover.contains(&s.game)
                        && (self.game == 0 || s.game == self.game) =>
                {
                    self.game = s.game;
                    self.players[who].tracker = GameTracker::new(s.game, s.you(), s.moves.iter().copied());
                    if self.players.iter().all(|p| p.tracker.is_some()) {
                        self.stats.latency("game_start", sent.elapsed());
                        self.stats.add("games_started", 1);
                        self.games_played += 1;
                        self.schedule_if_turn(0, cfg);
                        self.schedule_if_turn(1, cfg);
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }

    /// Schedules the move of `who` when it is their turn and none is scheduled.
    fn schedule_if_turn(&mut self, who: usize, cfg: &PlayConfig) {
        if self.next_move.is_some() || self.pending.is_some() {
            return;
        }
        if self.players[who].tracker.as_ref().is_some_and(GameTracker::is_my_turn) {
            let think = self.rng.jitter(cfg.move_interval, cfg.jitter);
            let think_ms = u32::try_from(think.as_millis()).unwrap_or(u32::MAX);
            self.next_move = Some((who, tokio::time::Instant::now() + think, think_ms));
        }
    }

    fn play_move(&mut self, cfg: &PlayConfig) -> Result<(), usize> {
        let Some((who, _, think_ms)) = self.next_move.take() else { return Ok(()) };
        let Some(tracker) = self.players[who].tracker.as_ref() else { return Ok(()) };
        if !tracker.is_my_turn() {
            return Ok(());
        }
        let game = tracker.game();
        if tracker.ply() >= cfg.max_plies {
            self.stats.add("resigns", 1);
            return self.players[who].conn.send(Cmd::Resign { game }).map(drop).map_err(|_| who);
        }
        let Some(mv) = tracker.random_move(&mut self.rng) else { return Ok(()) };
        let intent = tracker.move_msg(mv, think_ms);
        let ply = intent.ply;
        self.players[who]
            .conn
            .send(Cmd::Move { game, ply, mv, pos_hash: intent.pos_hash, think_ms })
            .map_err(|_| who)?;
        self.pending = Some((ply, Instant::now()));
        self.stats.add("moves_sent", 1);
        Ok(())
    }

    fn send_gestures(&mut self) -> Result<(), usize> {
        for (who, p) in self.players.iter_mut().enumerate() {
            let Some(tracker) = p.tracker.as_ref() else { continue };
            if tracker.is_over() {
                continue;
            }
            p.gesture_seq = p.gesture_seq.wrapping_add(1);
            let index = p.gesture_seq % RING;
            let yaw = index as i32 - (RING / 2) as i32;
            p.sent[index] = Some(Instant::now());
            let (game, ply) = (tracker.game(), tracker.ply());
            p.conn.send(Cmd::Gesture { game, ply, yaw, pitch: -400, lean: 30 }).map_err(|_| who)?;
            self.stats.add("gestures_sent", 1);
        }
        Ok(())
    }

    /// Handles one event of the current game; `true` once both players saw its end.
    fn on_event(&mut self, who: usize, event: Event, at: Instant, cfg: &PlayConfig) -> Result<bool, usize> {
        match event {
            Event::MoveMade { game, ply, mv } if game == self.game => {
                let Some(tracker) = self.players[who].tracker.as_mut() else { return Ok(false) };
                let is_mover = tracker.you() == color_of_ply(ply);
                if let Some((pending_ply, sent)) = self.pending
                    && pending_ply == ply
                {
                    if is_mover {
                        self.stats.latency("move_confirm", at.duration_since(sent));
                    } else {
                        self.stats.latency("move_relay", at.duration_since(sent));
                        self.stats.add("moves", 1);
                    }
                }
                match tracker.apply(ply, mv) {
                    Applied::Played | Applied::Known => {}
                    Applied::Gap | Applied::Illegal => {
                        self.stats.add("error.desync", 1);
                        self.players[who].conn.send(Cmd::Resync { game }).map_err(|_| who)?;
                    }
                }
                let done = self.players.iter().all(|p| p.tracker.as_ref().is_some_and(|t| t.ply() > ply));
                if done && self.pending.is_some_and(|(p, _)| p == ply) {
                    self.pending = None;
                }
                // The next move waits for both players to hold this one (its latencies are
                // measured against the one pending move).
                self.schedule_if_turn(0, cfg);
                self.schedule_if_turn(1, cfg);
            }
            Event::MoveRejected { game, code } if game == self.game => {
                self.stats.add(&format!("error.move_rejected_{code}"), 1);
                self.pending = None;
            }
            Event::Snapshot(s) if s.game == self.game => {
                let mut tracker = GameTracker::new(s.game, s.you(), s.moves.iter().copied());
                if s.over
                    && let Some(t) = tracker.as_mut()
                {
                    t.end();
                }
                self.players[who].tracker = tracker;
                self.pending = None;
                self.schedule_if_turn(who, cfg);
            }
            Event::GameEnd { game } if game == self.game => {
                let p = &mut self.players[who];
                p.ended = true;
                if let Some(t) = p.tracker.as_mut() {
                    t.end();
                }
                if self.players.iter().all(|p| p.ended) {
                    self.next_move = None;
                    self.stats.add("games_finished", 1);
                    return Ok(true);
                }
            }
            Event::Gesture { game, yaw } if game == self.game => {
                self.stats.add("gestures_relayed", 1);
                let sender = &mut self.players[1 - who];
                let index = yaw + (RING / 2) as i32;
                if let Ok(index) = usize::try_from(index)
                    && let Some(sent) = sender.sent.get_mut(index).and_then(Option::take)
                {
                    self.stats.latency("gesture_relay", at.duration_since(sent));
                }
            }
            Event::Error { code, .. } => self.stats.add(&format!("error.server_{code}"), 1),
            _ => {}
        }
        Ok(false)
    }

    /// Plays the current game to its end.
    pub async fn play(&mut self, cfg: &PlayConfig, stop: &mut watch::Receiver<bool>) -> Outcome {
        let mut gestures = cfg.gesture_period.map(|period| {
            // A random phase spreads the gestures of all the games over the period.
            let phase = period.mul_f64(self.rng.unit());
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            interval
        });
        loop {
            let result = match self.wake(gestures.as_mut(), stop).await {
                Wake::Stop => return Outcome::Stopped,
                Wake::Msg(who, Err(_)) => return self.lost(who),
                Wake::Msg(who, Ok((event, at))) => self.on_event(who, event, at, cfg),
                Wake::MoveDue => self.play_move(cfg).map(|()| false),
                Wake::GestureTick => self.send_gestures().map(|()| false),
            };
            match result {
                Ok(true) => return Outcome::Finished,
                Ok(false) => {}
                Err(who) => return self.lost(who),
            }
        }
    }

    /// Resigns the current game when it is still running, then closes both connections.
    pub async fn close(mut self) {
        let running =
            self.game != 0 && self.players.iter().any(|p| p.tracker.as_ref().is_some_and(|t| !t.is_over()));
        if running && self.players[0].conn.send(Cmd::Resign { game: self.game }).is_ok() {
            let game = self.game;
            let deadline = tokio::time::Instant::now() + END_TIMEOUT;
            // Wait for the end on the resigning side so the server has recorded it.
            while let Ok(Ok((event, _))) =
                tokio::time::timeout_at(deadline, self.players[0].conn.recv()).await
            {
                if matches!(event, Event::GameEnd { game: g } if g == game) {
                    break;
                }
            }
        }
        for p in &mut self.players {
            p.conn.close();
        }
        for p in &mut self.players {
            p.conn.wait_closed(Duration::from_secs(2)).await;
        }
    }
}

/// The task of one game slot: connect, then play games until the stop.
#[allow(clippy::too_many_arguments)]
async fn game_loop(
    ctx: Arc<Ctx>,
    a: Account,
    b: Account,
    index: u64,
    start_at: tokio::time::Instant,
    cfg: PlayConfig,
    stats: Arc<Stats>,
    mut stop: watch::Receiver<bool>,
    ready: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
) {
    tokio::select! {
        () = tokio::time::sleep_until(start_at) => {}
        _ = stop.changed() => {
            ready.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    let Some(mut pair) = Pair::connect(&ctx, &a, &b, stats.clone(), ctx.rng(index)).await else {
        ready.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut first = true;
    loop {
        if *stop.borrow() {
            break;
        }
        let started = pair.start(&cfg, &mut stop).await;
        if first {
            first = false;
            ready.fetch_add(1, Ordering::Relaxed);
        }
        let outcome = match started {
            Ok(()) => {
                live.fetch_add(1, Ordering::Relaxed);
                let outcome = pair.play(&cfg, &mut stop).await;
                live.fetch_sub(1, Ordering::Relaxed);
                outcome
            }
            Err(outcome) => outcome,
        };
        match outcome {
            Outcome::Finished | Outcome::NotStarted => {}
            Outcome::Stopped => break,
            Outcome::Lost => return,
        }
        let pause = ctx.opts.between_games;
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            _ = stop.changed() => break,
        }
    }
    pair.close().await;
}

/// The configuration of the games scenario.
pub fn play_config(ctx: &Ctx, gesture_rate: Option<u16>) -> PlayConfig {
    let o = &ctx.opts;
    let gestures = o.gesture_hz > 0.0 && gesture_rate != Some(0);
    PlayConfig {
        move_interval: o.move_interval,
        jitter: o.jitter,
        gesture_period: gestures.then(|| Duration::from_secs_f64(1.0 / o.gesture_hz)),
        max_plies: o.max_plies,
        tc: o.tc,
        rated: o.rated,
    }
}

/// Runs the scenario.
pub async fn run(ctx: &Arc<Ctx>) -> Result<(Value, Vec<Step>), String> {
    let max = ctx.opts.steps.iter().copied().max().unwrap_or(0);
    let accounts = ctx.need_accounts(2 * max)?.to_vec();
    // The gesture relay of the server (Welcome.gestureRate): one probe connection.
    let probe_stats = Stats::new();
    let probe = ctx
        .connect(&accounts[0].token, &probe_stats)
        .await
        .ok_or("cannot connect the first account (see above)")?;
    let gesture_rate = probe.welcome().gesture_rate;
    probe.close();
    if ctx.opts.gesture_hz > f64::from(gesture_rate) {
        progress(format!(
            "warning: --gesture-hz {} is above the server's gesture rate {gesture_rate}/s",
            ctx.opts.gesture_hz
        ));
    }
    let cfg = play_config(ctx, Some(gesture_rate));
    let mut steps = Vec::new();
    for &n in &ctx.opts.steps {
        progress(format!("games: starting {n} games"));
        let stats = Stats::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let ready = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let t0 = tokio::time::Instant::now();
        let mut set = JoinSet::new();
        for i in 0..n {
            let delay = Duration::from_secs_f64(i as f64 / ctx.opts.start_rate.max(0.001));
            set.spawn(game_loop(
                ctx.clone(),
                accounts[2 * i].clone(),
                accounts[2 * i + 1].clone(),
                i as u64,
                t0 + delay,
                cfg,
                stats.clone(),
                stop_rx.clone(),
                ready.clone(),
                live.clone(),
            ));
        }
        let ramp_limit = ctx.opts.connect_timeout
            + START_TIMEOUT
            + Duration::from_secs_f64(n as f64 / ctx.opts.start_rate.max(0.001))
            + Duration::from_secs(10);
        let ramp_deadline = t0 + ramp_limit;
        while ready.load(Ordering::Relaxed) < n && tokio::time::Instant::now() < ramp_deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let started = live.load(Ordering::Relaxed);
        progress(format!(
            "games: {started} of {n} playing after {:.1} s; measuring",
            t0.elapsed().as_secs_f64()
        ));
        let window = measure(ctx, &stats, ctx.opts.warmup, ctx.opts.duration).await;
        let live_at_end = live.load(Ordering::Relaxed);
        let _ = stop_tx.send(true);
        let _ =
            tokio::time::timeout(Duration::from_secs(60), async { while set.join_next().await.is_some() {} })
                .await;
        let errors: u64 =
            window.errors_json().as_object().map(|m| m.values().filter_map(Value::as_u64).sum()).unwrap_or(0);
        let step = Step::new(format!("{n} games"))
            .figure("live games", live_at_end)
            .figure("moves/s", window.rate("moves"))
            .figure("move relay p50 ms", window.latency("move_relay", "p50"))
            .figure("move relay p99 ms", window.latency("move_relay", "p99"))
            .figure("move confirm p99 ms", window.latency("move_confirm", "p99"))
            .figure("gestures/s", window.rate("gestures_relayed"))
            .figure("gesture p50 ms", window.latency("gesture_relay", "p50"))
            .figure("gesture p99 ms", window.latency("gesture_relay", "p99"))
            .figure("errors", errors)
            .figure("CPU %", window.cpu())
            .figure("RSS MiB", window.rss())
            .detail(
                json!({ "startedGames": started, "errors": window.errors_json(), "window": window.json() }),
            );
        steps.push(step);
        // Let the server record the resignations before the next step.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let params = json!({
        "steps": ctx.opts.steps,
        "moveIntervalMs": ctx.opts.move_interval.as_millis() as u64,
        "jitter": ctx.opts.jitter,
        "gestureHz": if cfg.gesture_period.is_some() { ctx.opts.gesture_hz } else { 0.0 },
        "serverGestureRate": gesture_rate,
        "maxPlies": ctx.opts.max_plies,
        "tc": format!("{}+{}", ctx.opts.tc.0 / 60, ctx.opts.tc.1),
        "rated": ctx.opts.rated,
        "betweenGamesMs": ctx.opts.between_games.as_millis() as u64,
        "startRate": ctx.opts.start_rate,
    });
    Ok((params, steps))
}
