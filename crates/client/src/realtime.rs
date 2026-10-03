//! The realtime protocol v1 (docs/PROTOCOL.md) over a WebSocket [`Session`]: the Hello/Welcome
//! exchange, typed messages in both directions with automatic `seq` numbering, automatic answers
//! to the server's heartbeat, and lenient decoding of what the server sends.

use std::time::{Duration, Instant};

use scacelith_protocol::{
    ClientMsg, ClientPong, DecodeError, Hello, MAX_SERVER_MESSAGE, MINOR, Message, MsgType, PROTOCOL_VERSION,
    SUBPROTOCOL, ServerMsg, ServerPing, Welcome,
};

use crate::error::{ClientError, CloseInfo, Result};
use crate::net::{ConnectTimings, Endpoint};
use crate::ws::{Session, SessionOptions};

/// Settings of a realtime connection.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// `Hello.client`: the client's name and version, for the server's logs (at most 48 bytes).
    pub client_name: String,
    /// `Hello.minor`: the highest minor version spoken.
    pub minor: u16,
    /// `Hello.caps`: the capability bits supported.
    pub caps: u64,
    /// Deadline from the `Hello` to the `Welcome`.
    pub hello_timeout: Duration,
    /// Answer the server's `Ping` with a `Pong` at once (the protocol requires it; turn it off
    /// only to test the server's heartbeat timeout). The ping is delivered either way.
    pub auto_pong: bool,
    /// The WebSocket settings (path, subprotocol, largest message, deadlines).
    pub session: SessionOptions,
}

impl Default for ConnectOptions {
    fn default() -> ConnectOptions {
        let mut session = SessionOptions::new(SUBPROTOCOL);
        session.max_message = MAX_SERVER_MESSAGE;
        ConnectOptions {
            client_name: concat!("scacelith-client/", env!("CARGO_PKG_VERSION")).to_string(),
            minor: MINOR,
            caps: 0,
            hello_timeout: Duration::from_secs(10),
            auto_pong: true,
            session,
        }
    }
}

/// The automatic answer to the server's `Ping`: a `Pong` with its nonce (the session gives it
/// its `seq`).
pub fn pong_reply(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.first() != Some(&MsgType::ServerPing.to_u8()) {
        return None;
    }
    let ping = ServerPing::decode(msg).ok()?;
    ClientPong { seq: 0, nonce: ping.nonce }.to_vec().ok()
}

/// A realtime connection that passed the Hello.
///
/// ```no_run
/// # async fn demo(endpoint: scacelith_client::Endpoint, token: &str) -> scacelith_client::Result<()> {
/// use scacelith_client::{ConnectOptions, Connection};
/// use scacelith_protocol::{QueueJoin, ServerMsg};
///
/// let mut conn = Connection::connect(&endpoint, token, &ConnectOptions::default()).await?;
/// println!("signed in as {}", conn.welcome().username);
/// let seq = conn.send(QueueJoin { seq: 0, category: "3+2".into(), rated: false })?;
/// loop {
///     match conn.recv().await? {
///         ServerMsg::Ack(ack) if ack.r#ref == seq => println!("queued"),
///         ServerMsg::GameSnapshot(game) => break println!("game {} starts", game.game),
///         _ => {}
///     }
/// }
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Connection {
    session: Session,
    welcome: Welcome,
    ignored: u64,
    last_decode_error: Option<DecodeError>,
}

impl Connection {
    /// Connects to `endpoint`, sends the `Hello` with the session token `token` and waits for the
    /// `Welcome`. A refusal is [`ClientError::Refused`] (with the close that followed it), or
    /// [`ClientError::UpgradeRefused`] when the upgrade itself was refused.
    pub async fn connect(endpoint: &Endpoint, token: &str, opts: &ConnectOptions) -> Result<Connection> {
        let session = Session::connect(endpoint, &Self::session_options(opts)).await?;
        Self::hello(session, token, opts).await
    }

    /// The session settings of `opts`, with the automatic pong when asked.
    pub fn session_options(opts: &ConnectOptions) -> SessionOptions {
        let mut session = opts.session.clone();
        if opts.auto_pong {
            session.auto_reply = Some(pong_reply);
        }
        session
    }

    /// Runs the Hello on an open session (made with [`Connection::session_options`], or any other
    /// settings for tests of the server).
    pub async fn hello(mut session: Session, token: &str, opts: &ConnectOptions) -> Result<Connection> {
        let hello = Hello {
            seq: 1,
            proto: PROTOCOL_VERSION,
            minor: opts.minor,
            caps: opts.caps,
            client: opts.client_name.clone(),
            token: token.to_string(),
        };
        let bytes = hello.to_vec()?;
        let started = Instant::now();
        session.send(&bytes)?;
        let deadline = tokio::time::Instant::now() + opts.hello_timeout;
        loop {
            let incoming = tokio::time::timeout_at(deadline, session.recv())
                .await
                .map_err(|_| ClientError::Timeout("Welcome"))??;
            match ServerMsg::decode(&incoming.payload) {
                Ok(Some(ServerMsg::Welcome(welcome))) => {
                    session.timings_mut().hello = started.elapsed();
                    return Ok(Connection { session, welcome, ignored: 0, last_decode_error: None });
                }
                Ok(Some(ServerMsg::Error(error))) => {
                    // A fatal Error is the server's last message: the close follows at once.
                    let close = if error.fatal {
                        tokio::time::timeout(Duration::from_secs(2), session.wait_closed()).await.ok()
                    } else {
                        None
                    };
                    return Err(ClientError::Refused { code: error.code, close });
                }
                // A Notice (Banned before its Error), a type of a later minor, an undecodable
                // message: not a Welcome yet.
                _ => {}
            }
        }
    }

    /// The `Welcome` of the connection.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Sends a message, numbered with the next `seq` (the `seq` it carries is replaced), and
    /// returns that `seq`: the `ref` of its `Ack` or `Error`.
    pub fn send(&self, msg: impl Into<ClientMsg>) -> Result<u32> {
        let msg = msg.into();
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf)?;
        self.session.send(&buf)
    }

    /// The next message from the server. Messages of a later minor (unknown types) and messages
    /// that do not decode are skipped and counted ([`Connection::ignored_messages`]). After the
    /// end of the connection: [`ClientError::Closed`]. Cancel-safe.
    pub async fn recv(&mut self) -> Result<ServerMsg> {
        Ok(self.recv_timed().await?.0)
    }

    /// [`Connection::recv`] with the instant the message was read from the socket.
    pub async fn recv_timed(&mut self) -> Result<(ServerMsg, Instant)> {
        loop {
            let incoming = self.session.recv().await?;
            match ServerMsg::decode(&incoming.payload) {
                Ok(Some(msg)) => return Ok((msg, incoming.at)),
                Ok(None) => self.ignored += 1,
                Err(e) => {
                    self.ignored += 1;
                    self.last_decode_error = Some(e);
                }
            }
        }
    }

    /// [`Connection::recv`] with a deadline ([`ClientError::Timeout`]).
    pub async fn recv_timeout(&mut self, limit: Duration) -> Result<ServerMsg> {
        tokio::time::timeout(limit, self.recv()).await.map_err(|_| ClientError::Timeout("server message"))?
    }

    /// Skips messages until `pick` returns `Some`, within `limit`. `what` names the awaited
    /// message in the timeout error.
    pub async fn expect<T>(
        &mut self,
        limit: Duration,
        what: &'static str,
        mut pick: impl FnMut(ServerMsg) -> Option<T>,
    ) -> Result<T> {
        let fut = async {
            loop {
                if let Some(found) = pick(self.recv().await?) {
                    return Ok(found);
                }
            }
        };
        tokio::time::timeout(limit, fut).await.map_err(|_| ClientError::Timeout(what))?
    }

    /// Closes the connection normally (1000) after the messages already sent.
    pub fn close(&self) {
        self.session.close(1000, "");
    }

    /// Closes the connection with another code.
    pub fn close_with(&self, code: u16, reason: &str) {
        self.session.close(code, reason);
    }

    /// Waits until the connection has ended and returns how.
    pub async fn wait_closed(&mut self) -> CloseInfo {
        self.session.wait_closed().await
    }

    /// How the connection ended, once it has.
    pub fn close_info(&self) -> Option<CloseInfo> {
        self.session.close_info()
    }

    /// The account of the connection.
    pub fn user_id(&self) -> u32 {
        self.welcome.user_id
    }

    /// The username of the connection.
    pub fn username(&self) -> &str {
        &self.welcome.username
    }

    /// Time spent connecting, Hello included.
    pub fn timings(&self) -> ConnectTimings {
        self.session.timings()
    }

    /// Server messages skipped by [`Connection::recv`] so far.
    pub fn ignored_messages(&self) -> u64 {
        self.ignored
    }

    /// Why the last skipped message did not decode.
    pub fn last_decode_error(&self) -> Option<DecodeError> {
        self.last_decode_error
    }

    /// The WebSocket session (raw frames, counters, upgrade headers).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The WebSocket session, mutably (raw receive).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The session, leaving the protocol layer.
    pub fn into_session(self) -> Session {
        self.session
    }
}
