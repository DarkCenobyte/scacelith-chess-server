//! Test doubles for the game module and for the modules that embed game hosts: scripted rules
//! (the chess rules do not matter to a room), and recording event and anomaly sinks. Not used by
//! the server itself.

use std::collections::HashMap;
use std::sync::Mutex;

use scacelith_protocol::{EndReason, ErrorCode, GameStatus, move_flag};
use tokio::sync::oneshot;

use crate::events::{Anomaly, AnomalySink, GameEnded, HostEvents, IncidentKind, RematchRequest};
use crate::ids::{GameId, UserId};

use super::rules::{Played, Rules, Side};

/// A move that is legal for [`FakeRules`] (origin and destination differ, bit 15 clear).
#[must_use]
pub fn fake_move(i: usize, promo: u16) -> u16 {
    let from = i & 63;
    // Offset 1..63: never the origin square.
    let to = (from + 1 + ((i >> 6) % 63)) & 63;
    (from | (to << 6)) as u16 | ((promo & 7) << 12)
}

/// What [`FakeRules`] does at given plies.
#[derive(Clone, Debug)]
pub struct Script {
    /// Moves that are always illegal.
    pub illegal: Vec<u16>,
    /// Extra `MoveFlag` bits returned when the move of that ply (0-based) is played.
    pub flags: HashMap<usize, u8>,
    /// Automatic end once that many plies are played.
    pub end_after: HashMap<usize, (GameStatus, EndReason)>,
    /// Ply counts at which a threefold repetition can be claimed.
    pub threefold_at: Vec<usize>,
    /// Ply counts at which the fifty-move rule can be claimed.
    pub fifty_at: Vec<usize>,
    /// Whether White / Black can still mate.
    pub can_mate: [bool; 2],
}

impl Default for Script {
    fn default() -> Self {
        Script {
            illegal: Vec::new(),
            flags: HashMap::new(),
            end_after: HashMap::new(),
            threefold_at: Vec::new(),
            fifty_at: Vec::new(),
            can_mate: [true, true],
        }
    }
}

/// Scripted rules: any move with distinct origin and destination squares and bit 15 clear is
/// legal while the game runs, unless the script says otherwise. The digest is an FNV-style hash
/// of the moves played; a move with promotion bits returns `MoveFlag.Promotion`.
#[derive(Clone, Debug)]
pub struct FakeRules {
    script: Script,
    moves: Vec<u16>,
    status: GameStatus,
    reason: EndReason,
    hash: u32,
}

impl FakeRules {
    /// Rules following `script`.
    #[must_use]
    pub fn new(script: Script) -> Self {
        FakeRules {
            script,
            moves: Vec::new(),
            status: GameStatus::Ongoing,
            reason: EndReason::None,
            hash: 0x811c_9dc5,
        }
    }

    /// Boxed rules following `script`, as rooms take them.
    #[must_use]
    pub fn boxed(script: Script) -> Box<dyn Rules> {
        Box::new(FakeRules::new(script))
    }

    /// Moves played so far.
    #[must_use]
    pub fn moves(&self) -> &[u16] {
        &self.moves
    }
}

impl Default for FakeRules {
    fn default() -> Self {
        FakeRules::new(Script::default())
    }
}

impl Rules for FakeRules {
    fn digest(&self) -> u32 {
        self.hash
    }

    fn is_legal(&self, m: u16) -> bool {
        self.status == GameStatus::Ongoing
            && m <= 0x7fff
            && (m & 63) != ((m >> 6) & 63)
            && !self.script.illegal.contains(&m)
    }

    fn play(&mut self, m: u16) -> Option<Played> {
        if !self.is_legal(m) {
            return None;
        }
        self.moves.push(m);
        let mut h = self.hash;
        h = (h ^ u32::from(m & 0xff)).wrapping_mul(0x0100_0193);
        h = (h ^ u32::from(m >> 8)).wrapping_mul(0x0100_0193);
        self.hash = h;
        let ply = self.moves.len() - 1;
        let mut flags = if (m >> 12) & 7 != 0 { move_flag::PROMOTION } else { 0 };
        if let Some(f) = self.script.flags.get(&ply) {
            flags |= f;
        }
        if let Some(&(status, reason)) = self.script.end_after.get(&self.moves.len()) {
            self.status = status;
            self.reason = reason;
        }
        Some(Played { flags, status: self.status, reason: self.reason })
    }

    fn status(&self) -> GameStatus {
        self.status
    }

    fn reason(&self) -> EndReason {
        self.reason
    }

    fn can_claim_threefold(&self) -> bool {
        self.script.threefold_at.contains(&self.moves.len())
    }

    fn can_claim_fifty_move(&self) -> bool {
        self.script.fifty_at.contains(&self.moves.len())
    }

    fn can_color_mate(&self, side: Side) -> bool {
        self.script.can_mate[side.index()]
    }

    fn end(&mut self, status: GameStatus, reason: EndReason) {
        self.status = status;
        self.reason = reason;
    }
}

/// One call received by a [`RecordingEvents`] or a [`RecordingAnomalies`].
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// [`HostEvents::game_ended`].
    GameEnded(GameEnded),
    /// [`HostEvents::game_recovered`].
    GameRecovered(GameId, UserId, UserId),
    /// [`HostEvents::rematch`] (the reply is answered by the double's rematch policy).
    Rematch(RematchRequest),
    /// [`HostEvents::conduct`].
    Conduct(UserId, IncidentKind),
    /// [`AnomalySink::record`].
    Anomaly(Anomaly),
    /// [`AnomalySink::sanction_certain`].
    Sanction(UserId, GameId, &'static str),
}

/// How a [`RecordingEvents`] answers rematch requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RematchPolicy {
    /// Answer with this game id.
    Accept(GameId),
    /// Answer with this error.
    Refuse(ErrorCode),
    /// Drop the reply sender without answering.
    #[default]
    Drop,
}

/// A [`HostEvents`] and [`AnomalySink`] that records every call in order.
#[derive(Debug, Default)]
pub struct RecordingEvents {
    calls: Mutex<Vec<Recorded>>,
    rematch: Mutex<RematchPolicy>,
}

impl RecordingEvents {
    /// A recorder answering rematch requests with `policy`.
    #[must_use]
    pub fn with_rematch(policy: RematchPolicy) -> Self {
        RecordingEvents { calls: Mutex::new(Vec::new()), rematch: Mutex::new(policy) }
    }

    /// Changes the rematch policy.
    pub fn set_rematch(&self, policy: RematchPolicy) {
        *self.rematch.lock().expect("recorder lock") = policy;
    }

    /// Every call so far.
    #[must_use]
    pub fn calls(&self) -> Vec<Recorded> {
        self.calls.lock().expect("recorder lock").clone()
    }

    /// Forgets the calls so far.
    pub fn clear(&self) {
        self.calls.lock().expect("recorder lock").clear();
    }

    /// The `game_ended` calls.
    #[must_use]
    pub fn ended(&self) -> Vec<GameEnded> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::GameEnded(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    /// The `conduct` calls.
    #[must_use]
    pub fn conduct(&self) -> Vec<(UserId, IncidentKind)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::Conduct(u, k) => Some((u, k)),
                _ => None,
            })
            .collect()
    }

    /// The recorded anomalies.
    #[must_use]
    pub fn anomalies(&self) -> Vec<Anomaly> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::Anomaly(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    /// The `sanction_certain` calls.
    #[must_use]
    pub fn sanctions(&self) -> Vec<(UserId, GameId, &'static str)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::Sanction(u, g, k) => Some((u, g, k)),
                _ => None,
            })
            .collect()
    }

    /// The rematch requests.
    #[must_use]
    pub fn rematches(&self) -> Vec<RematchRequest> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::Rematch(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// The `game_recovered` calls.
    #[must_use]
    pub fn recovered(&self) -> Vec<(GameId, UserId, UserId)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Recorded::GameRecovered(g, w, b) => Some((g, w, b)),
                _ => None,
            })
            .collect()
    }

    fn push(&self, call: Recorded) {
        self.calls.lock().expect("recorder lock").push(call);
    }
}

impl HostEvents for RecordingEvents {
    fn game_ended(&self, ended: GameEnded) {
        self.push(Recorded::GameEnded(ended));
    }

    fn game_recovered(&self, game: GameId, white: UserId, black: UserId) {
        self.push(Recorded::GameRecovered(game, white, black));
    }

    fn rematch(&self, request: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>) {
        self.push(Recorded::Rematch(request));
        match *self.rematch.lock().expect("recorder lock") {
            RematchPolicy::Accept(id) => {
                let _ = reply.send(Ok(id));
            }
            RematchPolicy::Refuse(code) => {
                let _ = reply.send(Err(code));
            }
            RematchPolicy::Drop => drop(reply),
        }
    }

    fn conduct(&self, user: UserId, kind: IncidentKind) {
        self.push(Recorded::Conduct(user, kind));
    }
}

impl AnomalySink for RecordingEvents {
    fn record(&self, anomaly: Anomaly) {
        self.push(Recorded::Anomaly(anomaly));
    }

    fn sanction_certain(&self, user: UserId, game: GameId, kind: &'static str) {
        self.push(Recorded::Sanction(user, game, kind));
    }
}

/// Alias kept for readability where only anomalies matter.
pub type RecordingAnomalies = RecordingEvents;
