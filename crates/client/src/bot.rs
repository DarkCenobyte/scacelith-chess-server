//! Bots: players that find games (matchmaking queue or challenges) and play random legal moves
//! at a chosen pace, with head gestures at a chosen rate. For integration tests and the load
//! generator.
//!
//! * [`GameTracker`] follows one game from the player's side with the chess rules of
//!   `scacelith-chess`: whose turn it is, a random legal move, the `Move` intent with its `ply`
//!   and `posHash`.
//! * [`Bot`] drives a [`Connection`]: `join_queue`, `challenge`, `wait_challenge`, `accept`,
//!   then `play` until the game ends.
//!
//! ```no_run
//! # async fn demo(alice: scacelith_client::Connection, bob: scacelith_client::Connection)
//! #     -> scacelith_client::Result<()> {
//! use scacelith_client::bot::{Bot, BotConfig};
//! use scacelith_protocol::ColorPref;
//!
//! let cfg = BotConfig { max_plies: 40, ..BotConfig::default() };
//! let (mut alice, mut bob) = (Bot::new(alice, cfg.clone()), Bot::new(bob, cfg));
//! alice.challenge("bob", 180, 2, false, ColorPref::Random).await?;
//! let id = bob.wait_challenge().await?.id;
//! let bob_game = bob.accept(id).await?;
//! let alice_game = alice.wait_game_start().await?;
//! let (a, b) = tokio::join!(alice.play(alice_game), bob.play(bob_game));
//! println!("{:?}", a?.end.reason);
//! # let _ = b; Ok(()) }
//! ```

use std::time::Duration;

use scacelith_chess::{ChessGame, Position};
use scacelith_protocol::{
    ChallengeAccept, ChallengeCreate, ChallengeReceived, ChallengeState, ClientGesture, Color, ColorPref,
    ErrorCode, GameEnd, GameSnapshot, Move, MoveMade, QueueJoin, Resign, ServerMsg,
};
use tokio::time::Instant;

use crate::error::{ClientError, Result};
use crate::realtime::Connection;

/// A small, fast pseudo-random generator (SplitMix64) for bots and load generation. Not for
/// anything secret.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator with a fixed seed (reproducible games).
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// A generator seeded from the operating system.
    pub fn from_entropy() -> Rng {
        Rng(getrandom::u64().expect("the operating system's random source is available"))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n` > 0).
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// A number in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `base` spread uniformly by `±spread` (0..=1): `base x (1 - spread + 2 spread u)`.
    pub fn jitter(&mut self, base: Duration, spread: f64) -> Duration {
        let spread = spread.clamp(0.0, 1.0);
        base.mul_f64(1.0 - spread + 2.0 * spread * self.unit())
    }
}

/// What [`GameTracker::apply`] did with a move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The move was the next ply: played.
    Played,
    /// The ply was already played (a duplicate).
    Known,
    /// The ply is ahead of the game: a move was missed (send a `Resync`).
    Gap,
    /// The move is illegal here: the tracker and the server disagree (send a `Resync`).
    Illegal,
}

/// One game seen from a player's side.
#[derive(Clone, Debug)]
pub struct GameTracker {
    game: u64,
    you: Color,
    chess: ChessGame,
    over: bool,
}

impl GameTracker {
    /// A game `game` where the player has colour `you`, after `moves`; `None` when a move cannot
    /// be replayed.
    pub fn new(game: u64, you: Color, moves: impl IntoIterator<Item = u16>) -> Option<GameTracker> {
        let moves: Vec<u16> = moves.into_iter().collect();
        let chess = ChessGame::from_moves(None, &moves)?;
        Some(GameTracker { game, you, chess, over: false })
    }

    /// The game of a `GameSnapshot`.
    pub fn from_snapshot(s: &GameSnapshot) -> Option<GameTracker> {
        let mut t = GameTracker::new(s.game, s.you, s.moves.iter().map(|m| m.r#move))?;
        t.over = s.status != scacelith_protocol::GameStatus::Ongoing;
        Some(t)
    }

    /// The game id.
    pub fn game(&self) -> u64 {
        self.game
    }

    /// The player's colour.
    pub fn you(&self) -> Color {
        self.you
    }

    /// Plies played.
    pub fn ply(&self) -> u16 {
        self.chess.ply() as u16
    }

    /// The current position.
    pub fn position(&self) -> &Position {
        self.chess.position()
    }

    /// Whether the game ended (by the rules, or a `GameEnd` was seen).
    pub fn is_over(&self) -> bool {
        self.over || self.chess.is_over()
    }

    /// Marks the game as ended.
    pub fn end(&mut self) {
        self.over = true;
    }

    /// Whether it is the player's turn in a running game.
    pub fn is_my_turn(&self) -> bool {
        let side = match self.chess.position().side() {
            scacelith_chess::Color::White => Color::White,
            scacelith_chess::Color::Black => Color::Black,
        };
        !self.is_over() && side == self.you
    }

    /// Applies the move of ply `ply`.
    pub fn apply(&mut self, ply: u16, mv: u16) -> Applied {
        let current = self.ply();
        if ply < current {
            return Applied::Known;
        }
        if ply > current {
            return Applied::Gap;
        }
        match self.chess.play(mv) {
            Ok(_) => Applied::Played,
            Err(_) => Applied::Illegal,
        }
    }

    /// Applies a `MoveMade` of this game (others are [`Applied::Known`]).
    pub fn apply_move_made(&mut self, m: &MoveMade) -> Applied {
        if m.game != self.game {
            return Applied::Known;
        }
        self.apply(m.ply, m.r#move)
    }

    /// A uniformly random legal move, `None` when there is none or the game is over.
    pub fn random_move(&self, rng: &mut Rng) -> Option<u16> {
        if self.is_over() {
            return None;
        }
        let moves = self.chess.position().legal_moves();
        (!moves.is_empty()).then(|| moves[rng.below(moves.len())])
    }

    /// The `Move` intent for `mv` in the current position.
    pub fn move_msg(&self, mv: u16, think_ms: u32) -> Move {
        Move {
            seq: 0,
            game: self.game,
            ply: self.ply(),
            r#move: mv,
            pos_hash: self.chess.position().digest(),
            think_ms,
            draw_offer: false,
        }
    }

    /// A `Gesture` of the head only (nothing in hand): `yaw` and `pitch` in milliradians (clamped
    /// to their bounds), `lean` in percent.
    pub fn gesture(&self, yaw: i32, pitch: i32, lean: u8) -> ClientGesture {
        ClientGesture {
            seq: 0,
            game: self.game,
            ply: self.ply().min(1199),
            touch: 64,
            aim: 64,
            placed: 0,
            flags: 0,
            yaw: yaw.clamp(-3142, 3142),
            pitch: pitch.clamp(-1571, 1571),
            lean: lean.min(100),
        }
    }
}

/// How a bot plays.
#[derive(Clone, Debug)]
pub struct BotConfig {
    /// Think time before each move.
    pub move_delay: Duration,
    /// Spread of the think time, 0..=1 (`move_delay x (1 ± jitter)`).
    pub move_jitter: f64,
    /// Head gestures per second while the game runs (0: none).
    pub gesture_hz: f64,
    /// Resign when the game reaches this many plies (0: play to the end).
    pub max_plies: u16,
    /// Seed of the move choice (`None`: from the operating system).
    pub seed: Option<u64>,
    /// Deadline of each wait (Ack, game start, challenge).
    pub timeout: Duration,
}

impl Default for BotConfig {
    fn default() -> BotConfig {
        BotConfig {
            move_delay: Duration::from_millis(100),
            move_jitter: 0.0,
            gesture_hz: 0.0,
            max_plies: 0,
            seed: None,
            timeout: Duration::from_secs(30),
        }
    }
}

/// The end of a game played by [`Bot::play`].
#[derive(Clone, Debug)]
pub struct GameResult {
    /// The `GameEnd`.
    pub end: GameEnd,
    /// Plies played.
    pub plies: u16,
    /// Moves the bot sent.
    pub moves_sent: u32,
    /// Moves refused (`MoveRejected`).
    pub moves_rejected: u32,
    /// Gestures sent.
    pub gestures_sent: u32,
    /// Opponent gestures received.
    pub gestures_received: u32,
}

/// A player driven by the program (see the module documentation).
#[derive(Debug)]
pub struct Bot {
    conn: Connection,
    cfg: BotConfig,
    rng: Rng,
}

impl Bot {
    /// A bot on an open connection.
    pub fn new(conn: Connection, cfg: BotConfig) -> Bot {
        let rng = cfg.seed.map_or_else(Rng::from_entropy, Rng::new);
        Bot { conn, cfg, rng }
    }

    /// The connection.
    pub fn connection(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The connection, leaving the bot.
    pub fn into_connection(self) -> Connection {
        self.conn
    }

    /// Waits for the `Ack` (or `Error`) of the request numbered `seq`.
    async fn ack(&mut self, seq: u32, what: &'static str) -> Result<()> {
        let answer = self
            .conn
            .expect(self.cfg.timeout, what, |m| match m {
                ServerMsg::Ack(a) if a.r#ref == seq => Some(Ok(())),
                ServerMsg::Error(e) if e.r#ref == seq => Some(Err(e.code)),
                _ => None,
            })
            .await?;
        answer.map_err(|code| ClientError::Unexpected(format!("{what} refused: {code:?}")))
    }

    /// Joins the matchmaking queue of `category` and waits for the game it finds.
    pub async fn join_queue(&mut self, category: &str, rated: bool) -> Result<GameTracker> {
        let seq = self.conn.send(QueueJoin { seq: 0, category: category.to_string(), rated })?;
        self.ack(seq, "QueueJoin").await?;
        self.wait_game_start().await
    }

    /// Challenges `target` (a username) and returns the challenge id, once it is pending.
    pub async fn challenge(
        &mut self,
        target: &str,
        base_sec: u16,
        inc_sec: u8,
        rated: bool,
        color: ColorPref,
    ) -> Result<u32> {
        let seq = self.conn.send(ChallengeCreate {
            seq: 0,
            target: target.to_string(),
            base_sec,
            inc_sec,
            rated,
            color,
        })?;
        self.ack(seq, "ChallengeCreate").await?;
        let status = self
            .conn
            .expect(self.cfg.timeout, "ChallengeStatus", |m| match m {
                ServerMsg::ChallengeStatus(s) => Some(s),
                _ => None,
            })
            .await?;
        match status.state {
            ChallengeState::Pending => Ok(status.id),
            other => Err(ClientError::Unexpected(format!("challenge {:?}", other))),
        }
    }

    /// Waits for a challenge from another player.
    pub async fn wait_challenge(&mut self) -> Result<ChallengeReceived> {
        self.conn
            .expect(self.cfg.timeout, "ChallengeReceived", |m| match m {
                ServerMsg::ChallengeReceived(c) => Some(c),
                _ => None,
            })
            .await
    }

    /// Accepts challenge `id` and waits for the game.
    pub async fn accept(&mut self, id: u32) -> Result<GameTracker> {
        let seq = self.conn.send(ChallengeAccept { seq: 0, id })?;
        self.ack(seq, "ChallengeAccept").await?;
        self.wait_game_start().await
    }

    /// Waits for the `GameSnapshot` of a game that starts (or resumes).
    pub async fn wait_game_start(&mut self) -> Result<GameTracker> {
        let snapshot = self
            .conn
            .expect(self.cfg.timeout, "GameSnapshot", |m| match m {
                ServerMsg::GameSnapshot(s) => Some(s),
                _ => None,
            })
            .await?;
        GameTracker::from_snapshot(&snapshot)
            .ok_or_else(|| ClientError::Unexpected("a snapshot whose moves cannot be replayed".into()))
    }

    /// Plays `game` until its `GameEnd`: a random legal move after the think time on each turn,
    /// a resignation at `max_plies`, gestures at `gesture_hz`.
    pub async fn play(&mut self, mut game: GameTracker) -> Result<GameResult> {
        let mut moves_sent = 0;
        let mut moves_rejected = 0;
        let mut gestures_sent = 0;
        let mut gestures_received = 0;
        let mut resigned = false;
        let gesture_period =
            (self.cfg.gesture_hz > 0.0).then(|| Duration::from_secs_f64(1.0 / self.cfg.gesture_hz));
        let mut next_gesture = gesture_period.map(|p| Instant::now() + p.mul_f64(self.rng.unit()));
        let mut move_at = self.schedule(&game);
        let mut turn_started = Instant::now();
        loop {
            tokio::select! {
                msg = self.conn.recv() => match msg? {
                    ServerMsg::MoveMade(m) if m.game == game.game() => match game.apply_move_made(&m) {
                        Applied::Played => {
                            turn_started = Instant::now();
                            move_at = self.schedule(&game);
                        }
                        Applied::Known => {}
                        Applied::Gap | Applied::Illegal => {
                            self.conn.send(scacelith_protocol::Resync { seq: 0, game: game.game() })?;
                            move_at = None;
                        }
                    },
                    ServerMsg::MoveRejected(r) if r.game == game.game() => {
                        // A GameSnapshot follows: wait for it before moving again.
                        moves_rejected += 1;
                        move_at = None;
                    }
                    ServerMsg::GameSnapshot(s) if s.game == game.game() => {
                        if let Some(fresh) = GameTracker::from_snapshot(&s) {
                            game = fresh;
                            move_at = self.schedule(&game);
                        }
                    }
                    ServerMsg::GameEnd(end) if end.game == game.game() => {
                        game.end();
                        return Ok(GameResult {
                            end,
                            plies: game.ply(),
                            moves_sent,
                            moves_rejected,
                            gestures_sent,
                            gestures_received,
                        });
                    }
                    ServerMsg::Gesture(g) if g.game == game.game() => gestures_received += 1,
                    ServerMsg::Error(e) if e.game == game.game() && e.code == ErrorCode::NotInGame => {
                        return Err(ClientError::Unexpected("the server says the bot is not in this game".into()));
                    }
                    _ => {}
                },
                () = sleep_until_opt(move_at) => {
                    move_at = None;
                    if !game.is_my_turn() {
                        continue;
                    }
                    let max = self.cfg.max_plies;
                    if max > 0 && game.ply() >= max && !resigned {
                        self.conn.send(Resign { seq: 0, game: game.game() })?;
                        resigned = true;
                    } else if let Some(mv) = game.random_move(&mut self.rng) {
                        let think = turn_started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                        self.conn.send(game.move_msg(mv, think))?;
                        moves_sent += 1;
                    }
                }
                () = sleep_until_opt(next_gesture) => {
                    let period = gesture_period.expect("gestures are scheduled only with a period");
                    let phase = gestures_sent as f64 * 0.3;
                    let yaw = (900.0 * phase.sin()) as i32;
                    let pitch = (-300.0 + 200.0 * (0.7 * phase).sin()) as i32;
                    self.conn.send(game.gesture(yaw, pitch, 20))?;
                    gestures_sent += 1;
                    next_gesture = next_gesture.map(|at| (at + period).max(Instant::now()));
                }
            }
        }
    }

    /// When to move next: after the think time when it is the bot's turn.
    fn schedule(&mut self, game: &GameTracker) -> Option<Instant> {
        game.is_my_turn().then(|| Instant::now() + self.rng.jitter(self.cfg.move_delay, self.cfg.move_jitter))
    }
}

/// Sleeps until `deadline`, forever without one.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scacelith_protocol::fen_digest;

    #[test]
    fn rng_is_reproducible_and_bounded() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        for _ in 0..1000 {
            assert!(a.below(13) < 13);
            let u = a.unit();
            assert!((0.0..1.0).contains(&u));
            let d = a.jitter(Duration::from_millis(1000), 0.5);
            assert!((500..=1500).contains(&d.as_millis()));
        }
        assert_eq!(Rng::new(1).jitter(Duration::from_millis(80), 0.0), Duration::from_millis(80));
    }

    #[test]
    fn tracker_follows_a_game() {
        let mut white = GameTracker::new(42, Color::White, []).unwrap();
        let mut black = GameTracker::new(42, Color::Black, []).unwrap();
        assert!(white.is_my_turn() && !black.is_my_turn());
        let msg = white.move_msg(0x070c, 5);
        assert_eq!((msg.ply, msg.pos_hash, msg.game), (0, 0x3706_291C, 42));
        assert_eq!(white.apply(0, 0x070c), Applied::Played);
        assert_eq!(black.apply(0, 0x070c), Applied::Played);
        assert_eq!(black.apply(0, 0x070c), Applied::Known);
        assert_eq!(black.apply(5, 0x070c), Applied::Gap);
        assert_eq!(black.apply(1, 0x070c), Applied::Illegal);
        assert!(black.is_my_turn() && !white.is_my_turn());
        let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq -";
        assert_eq!(black.move_msg(0, 0).pos_hash, fen_digest(fen));

        // Random games stay legal until the end.
        let mut rng = Rng::new(3);
        let mut game = GameTracker::new(1, Color::White, []).unwrap();
        while let Some(mv) = game.random_move(&mut rng) {
            let ply = game.ply();
            assert_eq!(game.apply(ply, mv), Applied::Played);
            if ply > 600 {
                break;
            }
        }
        assert!(game.is_over() || game.ply() > 600);
        let g = game.gesture(5000, -5000, 200);
        assert_eq!((g.yaw, g.pitch, g.lean, g.touch), (3142, -1571, 100, 64));
        assert!(GameTracker::new(1, Color::White, [0x070c, 0x070c]).is_none());
    }
}
