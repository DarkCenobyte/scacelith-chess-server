//! What the lobby holds of an authenticated connection: its endpoint, the hash of its session
//! token, whether its `Welcome` is out, and a command channel to its task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bytes::Bytes;
use tokio::sync::mpsc;

use super::endpoint::Endpoint;
use crate::ids::{ConnId, GameId, UserId};

/// Lobby requests one connection may have in flight; more are answered `RateLimited`.
pub(crate) const MAX_LOBBY_IN_FLIGHT: u32 = 8;

/// A command for a connection task.
#[derive(Debug)]
pub(crate) enum ConnCmd {
    /// Bind the connection to a game (the host then sends the snapshot). Sent by the lobby when it
    /// creates a game for the player; handled after the `Welcome`.
    Attach(GameId),
}

/// An authenticated connection, as the lobby sees it (presence, notifications, kicks).
#[derive(Debug)]
pub(crate) struct ConnLink {
    endpoint: Endpoint,
    username: String,
    token_hash: [u8; 32],
    welcomed: AtomicBool,
    in_flight: AtomicU32,
    /// Unbounded: the lobby sends one command per game it creates for the player, and a player
    /// starts no game while one is starting or in progress.
    cmds: mpsc::UnboundedSender<ConnCmd>,
}

impl ConnLink {
    /// A link for the connection of `endpoint`, with the receiving end of its commands.
    pub(crate) fn new(
        endpoint: Endpoint,
        username: String,
        token_hash: [u8; 32],
    ) -> (Arc<ConnLink>, mpsc::UnboundedReceiver<ConnCmd>) {
        let (cmds, rx) = mpsc::unbounded_channel();
        let link = ConnLink {
            endpoint,
            username,
            token_hash,
            welcomed: AtomicBool::new(false),
            in_flight: AtomicU32::new(0),
            cmds,
        };
        (Arc::new(link), rx)
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub(crate) fn user_id(&self) -> UserId {
        self.endpoint.user_id()
    }

    pub(crate) fn conn_id(&self) -> ConnId {
        self.endpoint.conn_id()
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn token_hash(&self) -> &[u8; 32] {
        &self.token_hash
    }

    /// Queues a frame for the player (see [`Endpoint::send`]).
    pub(crate) fn send(&self, frame: Bytes) -> bool {
        self.endpoint.send(frame)
    }

    /// Kicks the connection (see [`Endpoint::kick`]).
    pub(crate) fn kick(&self, frames: &[Bytes], code: u16) -> bool {
        self.endpoint.kick(frames, code, "")
    }

    /// Whether the `Welcome` was written: frames queued from now on reach the player unless the
    /// connection closes.
    pub(crate) fn is_welcomed(&self) -> bool {
        self.welcomed.load(Ordering::Acquire)
    }

    pub(crate) fn set_welcomed(&self) {
        self.welcomed.store(true, Ordering::Release);
    }

    /// Counts a lobby request in flight; false (nothing counted) when the connection already has
    /// [`MAX_LOBBY_IN_FLIGHT`].
    pub(crate) fn begin_request(&self) -> bool {
        self.in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_LOBBY_IN_FLIGHT).then_some(n + 1))
            .is_ok()
    }

    /// A lobby request was answered.
    pub(crate) fn end_request(&self) {
        let _ = self.in_flight.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }

    /// Sends a command to the connection task (dropped when the task is gone).
    pub(crate) fn command(&self, cmd: ConnCmd) {
        let _ = self.cmds.send(cmd);
    }
}
