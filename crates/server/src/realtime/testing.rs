//! Test doubles of the realtime layer: game hosts that record what they are told, a token
//! validator with a fixed table, and helpers to read the frames of an outbound queue.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use scacelith_protocol::{ClientMsg, ErrorCode, MsgType, ServerMsg};

use super::deps::{BoxFuture, GameHosts, Session, TokenValidator, ValidateError};
use super::endpoint::{Endpoint, Outbound};
use crate::clock::Clock;
use crate::events::{Anomaly, AnomalySink, NewGame};
use crate::ids::{self, ConnId, GameId, GameIdAllocator, UserId};

/// What a [`FakeHosts`] was told.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HostCall {
    Client { game: GameId, user: UserId, kind: MsgType, recv_at: f64 },
    Gesture { game: GameId, user: UserId, frame: Bytes },
    Attach { game: GameId, user: UserId, conn: ConnId },
    Detach { game: GameId, user: UserId, conn: ConnId },
    Rtt { game: GameId, user: UserId, rtt_ms: u32 },
    Forfeit { game: GameId, user: UserId },
    DeclineRematch { game: GameId, user: UserId },
    Create { preferred: Option<u32>, game: NewGame },
    Cancel { game: GameId },
}

/// How [`FakeHosts::create`] answers.
#[derive(Clone, Debug)]
pub(crate) enum CreateMode {
    /// A new game id on the preferred shard (0 without one).
    Ok,
    /// This error.
    Fail(ErrorCode),
    /// Never answers.
    Hang,
    /// Answers with a new game id once the gate is opened.
    Late(Arc<tokio::sync::Notify>),
}

/// Game hosts for shards `0..shards` that record every call.
pub(crate) struct FakeHosts {
    shards: u32,
    calls: Mutex<Vec<HostCall>>,
    alloc: Mutex<HashMap<u32, GameIdAllocator>>,
    mode: Mutex<CreateMode>,
    /// Endpoints attached, for the tests that write as a host.
    endpoints: Mutex<HashMap<(GameId, UserId), Endpoint>>,
    stalled: AtomicBool,
    alive: AtomicBool,
}

impl FakeHosts {
    pub(crate) fn new(shards: u32) -> Arc<FakeHosts> {
        Arc::new(FakeHosts {
            shards,
            calls: Mutex::new(Vec::new()),
            alloc: Mutex::new(HashMap::new()),
            mode: Mutex::new(CreateMode::Ok),
            endpoints: Mutex::new(HashMap::new()),
            stalled: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        })
    }

    pub(crate) fn set_mode(&self, mode: CreateMode) {
        *self.mode.lock() = mode;
    }

    pub(crate) fn set_stalled(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::SeqCst);
    }

    pub(crate) fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::SeqCst);
    }

    pub(crate) fn calls(&self) -> Vec<HostCall> {
        self.calls.lock().clone()
    }

    pub(crate) fn clear(&self) {
        self.calls.lock().clear();
    }

    /// The games created, in order.
    pub(crate) fn created(&self) -> Vec<NewGame> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                HostCall::Create { game, .. } => Some(game),
                _ => None,
            })
            .collect()
    }

    /// The endpoint attached to a game for a user.
    pub(crate) fn endpoint(&self, game: GameId, user: UserId) -> Option<Endpoint> {
        self.endpoints.lock().get(&(game, user)).cloned()
    }

    /// A new game id of `shard`.
    pub(crate) fn next_id(&self, shard: u32) -> GameId {
        self.alloc
            .lock()
            .entry(shard)
            .or_insert_with(|| GameIdAllocator::new(shard))
            .next(crate::clock::wall_ms())
    }

    fn record(&self, call: HostCall) {
        self.calls.lock().push(call);
    }

    fn hosts(&self, game: GameId) -> bool {
        ids::is_game_id(game) && ids::shard_of(game) < self.shards
    }
}

impl GameHosts for FakeHosts {
    fn client(&self, game: GameId, user: UserId, msg: ClientMsg, _ep: Endpoint, recv_at: f64) -> bool {
        if !self.hosts(game) {
            return false;
        }
        self.record(HostCall::Client { game, user, kind: msg.msg_type(), recv_at });
        true
    }

    fn gesture(&self, game: GameId, user: UserId, frame: Bytes) -> bool {
        if !self.hosts(game) {
            return false;
        }
        self.record(HostCall::Gesture { game, user, frame });
        true
    }

    fn attach(&self, game: GameId, user: UserId, ep: Endpoint) {
        self.record(HostCall::Attach { game, user, conn: ep.conn_id() });
        self.endpoints.lock().insert((game, user), ep);
    }

    fn detach(&self, game: GameId, user: UserId, conn: ConnId) {
        self.record(HostCall::Detach { game, user, conn });
    }

    fn rtt(&self, game: GameId, user: UserId, rtt_ms: u32) {
        self.record(HostCall::Rtt { game, user, rtt_ms });
    }

    fn forfeit_user(&self, game: GameId, user: UserId) {
        self.record(HostCall::Forfeit { game, user });
    }

    fn decline_rematch(&self, game: GameId, user: UserId) {
        self.record(HostCall::DeclineRematch { game, user });
    }

    fn create(&self, preferred: Option<u32>, game: NewGame) -> BoxFuture<Result<GameId, ErrorCode>> {
        self.record(HostCall::Create { preferred, game });
        let mode = self.mode.lock().clone();
        match mode {
            CreateMode::Ok => {
                let id = self.next_id(preferred.unwrap_or(0));
                Box::pin(async move { Ok(id) })
            }
            CreateMode::Fail(code) => Box::pin(async move { Err(code) }),
            CreateMode::Hang => Box::pin(std::future::pending()),
            CreateMode::Late(gate) => {
                let id = self.next_id(preferred.unwrap_or(0));
                Box::pin(async move {
                    gate.notified().await;
                    Ok(id)
                })
            }
        }
    }

    fn cancel(&self, game: GameId) {
        self.record(HostCall::Cancel { game });
    }

    fn stall_during(&self, _since_mono_ms: f64) -> bool {
        self.stalled.load(Ordering::SeqCst)
    }

    fn ping(&self) -> BoxFuture<bool> {
        let alive = self.alive.load(Ordering::SeqCst);
        Box::pin(async move { alive })
    }
}

/// A token validator over a table of tokens.
#[derive(Default)]
pub(crate) struct FakeTokens {
    sessions: Mutex<HashMap<String, Session>>,
    fail: AtomicBool,
    /// Tokens validated, in order.
    seen: Mutex<Vec<String>>,
    /// Validations left for a token, after which it is unknown.
    left: Mutex<HashMap<String, u32>>,
    /// The next validation waits for this gate to open.
    gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl FakeTokens {
    pub(crate) fn new() -> Arc<FakeTokens> {
        Arc::new(FakeTokens::default())
    }

    /// Adds a session for `token`.
    pub(crate) fn add(&self, token: &str, user_id: UserId, username: &str, email_verified: bool) {
        let session = Session {
            user_id,
            username: username.to_string(),
            session_id: i64::from(user_id),
            email_verified,
            token_hash: crate::util::sha256(token.as_bytes()),
        };
        self.sessions.lock().insert(token.to_string(), session);
    }

    pub(crate) fn revoke(&self, token: &str) {
        self.sessions.lock().remove(token);
    }

    /// The token is valid for `n` more validations only (a revocation during the Hello).
    pub(crate) fn valid_times(&self, token: &str, n: u32) {
        self.left.lock().insert(token.to_string(), n);
    }

    /// The next validation answers once `gate` is notified.
    pub(crate) fn hold_next(&self, gate: Arc<tokio::sync::Notify>) {
        *self.gate.lock() = Some(gate);
    }

    pub(crate) fn set_failing(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    pub(crate) fn seen(&self) -> Vec<String> {
        self.seen.lock().clone()
    }
}

impl TokenValidator for FakeTokens {
    fn validate(&self, token: String) -> BoxFuture<Result<Option<Session>, ValidateError>> {
        self.seen.lock().push(token.clone());
        let expired = match self.left.lock().get_mut(&token) {
            Some(0) => true,
            Some(n) => {
                *n -= 1;
                false
            }
            None => false,
        };
        let result = if self.fail.load(Ordering::SeqCst) {
            Err(ValidateError::from("the store is down"))
        } else if expired {
            Ok(None)
        } else {
            Ok(self.sessions.lock().get(&token).cloned())
        };
        let gate = self.gate.lock().take();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            result
        })
    }
}

/// Anomalies and certain-cheat sanctions, recorded.
#[derive(Default)]
pub(crate) struct RecordingAnomalies {
    anomalies: Mutex<Vec<Anomaly>>,
    sanctions: Mutex<Vec<(UserId, GameId, &'static str)>>,
}

impl RecordingAnomalies {
    pub(crate) fn new() -> Arc<RecordingAnomalies> {
        Arc::new(RecordingAnomalies::default())
    }

    pub(crate) fn anomalies(&self) -> Vec<Anomaly> {
        self.anomalies.lock().clone()
    }

    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        self.anomalies.lock().iter().map(|a| a.kind).collect()
    }

    pub(crate) fn sanctions(&self) -> Vec<(UserId, GameId, &'static str)> {
        self.sanctions.lock().clone()
    }
}

impl AnomalySink for RecordingAnomalies {
    fn record(&self, anomaly: Anomaly) {
        self.anomalies.lock().push(anomaly);
    }

    fn sanction_certain(&self, user: UserId, game: GameId, kind: &'static str) {
        self.sanctions.lock().push((user, game, kind));
    }
}

/// A monotonic clock that follows tokio's clock, so that the connection tasks' timers (tokio)
/// and timestamps (the clock) agree under paused time. The wall clock moves with it.
#[derive(Debug)]
pub(crate) struct TokioClock {
    start: tokio::time::Instant,
    mono0: f64,
    wall0: i64,
}

impl TokioClock {
    pub(crate) fn new(mono0: f64, wall0: i64) -> Arc<TokioClock> {
        Arc::new(TokioClock { start: tokio::time::Instant::now(), mono0, wall0 })
    }
}

impl Clock for TokioClock {
    fn mono_ms(&self) -> f64 {
        self.mono0 + self.start.elapsed().as_secs_f64() * 1000.0
    }

    fn wall_ms(&self) -> i64 {
        self.wall0 + i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX)
    }
}

/// Takes and decodes every frame queued on `out` (the close request is left in place).
pub(crate) fn drain(out: &Outbound) -> Vec<ServerMsg> {
    let batch = out.take();
    let bytes: usize = batch.frames.iter().map(Bytes::len).sum();
    out.written(bytes);
    batch.frames.iter().map(|f| decode(f)).collect()
}

/// Decodes a server frame.
pub(crate) fn decode(frame: &[u8]) -> ServerMsg {
    ServerMsg::decode_exact(frame).expect("a valid server message")
}

/// The name of a server message, for compact assertions.
pub(crate) fn name(msg: &ServerMsg) -> &'static str {
    match msg {
        ServerMsg::Welcome(_) => "Welcome",
        ServerMsg::Error(_) => "Error",
        ServerMsg::Ping(_) => "S_Ping",
        ServerMsg::Pong(_) => "S_Pong",
        ServerMsg::Ack(_) => "Ack",
        ServerMsg::Notice(_) => "Notice",
        ServerMsg::QueueStatus(_) => "QueueStatus",
        ServerMsg::ChallengeReceived(_) => "ChallengeReceived",
        ServerMsg::ChallengeStatus(_) => "ChallengeStatus",
        ServerMsg::GameSnapshot(_) => "GameSnapshot",
        ServerMsg::MoveMade(_) => "MoveMade",
        ServerMsg::MoveRejected(_) => "MoveRejected",
        ServerMsg::GameEvent(_) => "GameEvent",
        ServerMsg::GameEnd(_) => "GameEnd",
        ServerMsg::RatingUpdate(_) => "RatingUpdate",
        ServerMsg::Gesture(_) => "S_Gesture",
    }
}
