//! The Hello of a connection, up to its `Welcome` (see the module documentation of
//! [`super`]). Before the `Welcome` the connection task writes to the socket itself; the outbound
//! queue and its writer task take over after it.

use std::sync::Arc;

use bytes::Bytes;
use scacelith_protocol::{
    CAPS, ErrorCode, Hello, HelloPrefix, MINOR, Message, MsgType, NoticeCode, PROTOCOL_VERSION, Welcome,
    close_code_for, decode_hello, peek_seq,
};
use tokio::sync::{mpsc, oneshot};

use super::{ClaimGuard, ConnContext, MAX_PENDING_HELLO};
use crate::ids::{ConnId, GameId};
use crate::log;
use crate::net::ws::{CLOSE_GOING_AWAY, CLOSE_INTERNAL, WsEvent, WsInfo, WsReader, WsWriter};
use crate::realtime::drain::{DrainPhase, DrainWatch};
use crate::realtime::endpoint::{Endpoint, Outbound};
use crate::realtime::frames;
use crate::realtime::link::{ConnCmd, ConnLink};
use crate::realtime::lobby::{ClaimOutcome, LobbyMsg};
use crate::realtime::metrics;
use crate::realtime::reads;
use crate::{log_error, log_warn};

/// An authenticated connection whose `Welcome` is written.
#[derive(Debug)]
pub(crate) struct Welcomed {
    pub link: Arc<ConnLink>,
    pub out: Arc<Outbound>,
    pub cmds: mpsc::UnboundedReceiver<ConnCmd>,
    pub claim: ClaimGuard,
    /// The game in progress to attach (0: none).
    pub active_game: GameId,
    /// Messages received between the Hello and the `Welcome`.
    pub pending: Vec<Bytes>,
}

/// A refused Hello: the fatal `Error` (after its `Notice`, if any) and the `scacelith_ws_hello_total`
/// label.
#[derive(Debug)]
pub(crate) struct Refusal {
    label: &'static str,
    r#ref: u32,
    code: ErrorCode,
    notice: Option<Bytes>,
}

impl Refusal {
    pub(crate) fn new(label: &'static str, r#ref: u32, code: ErrorCode) -> Refusal {
        Refusal { label, r#ref, code, notice: None }
    }

    fn with_notice(mut self, code: NoticeCode, arg: f64) -> Refusal {
        self.notice = Some(frames::notice(code, arg));
        self
    }

    fn internal() -> Refusal {
        Refusal::new("internal", 1, ErrorCode::Internal)
    }
}

/// Runs the Hello. `None` when the connection ended or was refused (its close has begun).
pub(crate) async fn run(
    ctx: &Arc<ConnContext>,
    reader: &mut WsReader,
    writer: &mut WsWriter,
    info: &WsInfo,
    drain: &mut DrainWatch,
) -> Option<Welcomed> {
    let s = &ctx.settings;
    let opened = info.opened_at_ms();
    let first = tokio::select! {
        biased;
        ev = reader.next() => Some(ev),
        () = tokio::time::sleep_until(ctx.instant_at(opened + s.hello_timeout_ms)) => None,
        () = drain.closing() => {
            refuse(ctx, writer, Refusal::new("shutting_down", 0, ErrorCode::ShuttingDown)).await;
            return None;
        }
    };
    let buf = match first {
        None => {
            refuse(ctx, writer, Refusal::new("timeout", 0, ErrorCode::HelloRequired)).await;
            return None;
        }
        Some(WsEvent::Closed(_)) => return None,
        Some(WsEvent::Message(buf)) => buf,
    };
    let hello = match check(&buf) {
        Ok(hello) => hello,
        Err(refusal) => {
            refuse(ctx, writer, refusal).await;
            return None;
        }
    };

    // Authentication; the messages that arrive meanwhile wait for the `Welcome`.
    let auth = authenticate(ctx, info.id(), &hello);
    tokio::pin!(auth);
    let mut pending = Vec::new();
    let stuck_at = ctx.instant_at(opened + 2.0 * s.hello_timeout_ms);
    let admitted = loop {
        let silent_at = ctx.instant_at(info.last_recv_ms() + s.heartbeat_timeout_ms);
        tokio::select! {
            biased;
            result = &mut auth => match result {
                Ok(admitted) => break admitted,
                Err(refusal) => {
                    refuse(ctx, writer, refusal).await;
                    return None;
                }
            },
            ev = reader.next() => match ev {
                WsEvent::Message(b) if pending.len() < MAX_PENDING_HELLO => pending.push(b),
                WsEvent::Message(_) => {
                    refuse(ctx, writer, Refusal::new("flood", 0, ErrorCode::Flood)).await;
                    return None;
                }
                WsEvent::Closed(_) => return None,
            },
            () = tokio::time::sleep_until(stuck_at) => {
                // The auth service or the lobby does not answer.
                log_warn!(ctx.log, "hello stuck", { "connId": info.id() });
                refuse(ctx, writer, Refusal::internal()).await;
                return None;
            }
            () = tokio::time::sleep_until(silent_at) => {
                if ctx.clock.mono_ms() - info.last_recv_ms() >= s.heartbeat_timeout_ms {
                    info.close(CLOSE_GOING_AWAY, "timeout");
                    return None;
                }
            }
            () = drain.closing() => {
                refuse(ctx, writer, Refusal::new("shutting_down", 0, ErrorCode::ShuttingDown)).await;
                return None;
            }
        }
    };
    let Admitted { link, out, cmds, claim, active_game } = admitted;

    if drain.current() != DrainPhase::Running {
        refuse(ctx, writer, Refusal::new("shutting_down", 0, ErrorCode::ShuttingDown)).await;
        return None;
    }
    // Kicked before its `Welcome` (replaced, banned or revoked meanwhile): the kick's frames only.
    if !out.is_open() {
        let batch = out.take_kick();
        metrics::conn().hello.with(&["kicked"]).inc();
        write_direct(ctx, writer, &batch.frames).await;
        match batch.close {
            Some(close) => writer.close(close.code, &close.reason),
            None => writer.close(CLOSE_INTERNAL, ""),
        }
        return None;
    }
    let welcome = Welcome {
        proto: PROTOCOL_VERSION,
        minor: negotiated_minor(hello.minor),
        caps: hello.caps & CAPS,
        server_time: ctx.clock.mono_ms(),
        user_id: link.user_id(),
        username: link.username().to_string(),
        server_name: s.server_name.clone(),
        heartbeat_ms: s.heartbeat_interval_ms as u32,
        client_ping_ms: s.client_ping_ms,
        max_msg_per_sec: s.msg_rate.min(65535.0) as u16,
        msg_burst: s.msg_burst.min(65535.0) as u16,
        active_game,
        gesture_rate: s.gesture_rate as u16,
        gesture_burst: if s.gesture_rate > 0.0 { s.gesture_burst as u16 } else { 0 },
    };
    let welcome = match welcome.to_bytes() {
        Ok(frame) => frame,
        Err(e) => {
            log_error!(ctx.log, "welcome not encodable", { "err": e.to_string(), "userId": link.user_id() });
            refuse(ctx, writer, Refusal::internal()).await;
            return None;
        }
    };
    if !write_direct(ctx, writer, &[welcome]).await {
        return None;
    }
    link.set_welcomed();
    let m = metrics::conn();
    m.hello.with(&["ok"]).inc();
    m.hello_ms.observe(ctx.clock.mono_ms() - opened);
    Some(Welcomed { link, out, cmds, claim, active_game, pending })
}

/// The minor version of the session: the lower of the client's and the server's.
#[allow(clippy::unnecessary_min_or_max, reason = "the server's MINOR is 0 for now; later minors negotiate")]
fn negotiated_minor(client: u16) -> u16 {
    client.min(MINOR)
}

/// The checks of the Hello's bytes, in the order of PROTOCOL.md.
fn check(buf: &[u8]) -> Result<Hello, Refusal> {
    if buf.first() != Some(&MsgType::Hello.to_u8()) {
        return Err(Refusal::new("hello_required", 0, ErrorCode::HelloRequired));
    }
    let Some(prefix) = HelloPrefix::read(buf) else {
        return Err(Refusal::new("malformed", peek_seq(buf).unwrap_or(0), ErrorCode::Malformed));
    };
    if prefix.proto != PROTOCOL_VERSION {
        return Err(Refusal::new("unsupported_protocol", prefix.seq, ErrorCode::UnsupportedProtocol));
    }
    let hello = decode_hello(buf).map_err(|_| Refusal::new("malformed", prefix.seq, ErrorCode::Malformed))?;
    if hello.seq != 1 {
        return Err(Refusal::new("malformed", hello.seq, ErrorCode::ProtocolViolation));
    }
    Ok(hello)
}

/// A connection the lobby admitted.
struct Admitted {
    link: Arc<ConnLink>,
    out: Arc<Outbound>,
    cmds: mpsc::UnboundedReceiver<ConnCmd>,
    claim: ClaimGuard,
    active_game: GameId,
}

/// Validates the token, claims presence, validates the token again. Dropped midway, it leaves no
/// claim behind: the lobby releases a claim whose answer it cannot deliver, and the guard of an
/// answered one releases it.
async fn authenticate(ctx: &ConnContext, conn: ConnId, hello: &Hello) -> Result<Admitted, Refusal> {
    let session = match ctx.tokens.validate(hello.token.clone()).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(Refusal::new("unauthorized", 1, ErrorCode::Unauthorized)),
        Err(e) => {
            log_error!(ctx.log, "token validation failed", { "err": log::error_value("ValidateError", &e.to_string(), None, None) });
            return Err(Refusal::internal());
        }
    };
    if ctx.settings.require_email && !session.email_verified {
        return Err(Refusal::new("email_unverified", 1, ErrorCode::EmailUnverified));
    }
    let user = session.user_id;
    let ban = reads::stored_ban(&ctx.store, user, ctx.clock.wall_ms(), &ctx.log).await;
    let out = Outbound::new(ctx.settings.send_buffer_limit);
    let endpoint = Endpoint::new(conn, user, out.clone());
    let (link, cmds) = ConnLink::new(endpoint, session.username, session.token_hash);
    let (reply, answer) = oneshot::channel();
    ctx.lobby.post(LobbyMsg::Claim { link: link.clone(), ban, reply });
    let active_game = match answer.await {
        Ok(ClaimOutcome::Admitted { active_game }) => active_game,
        Ok(ClaimOutcome::Banned { until }) => {
            return Err(
                Refusal::new("banned", 1, ErrorCode::Banned).with_notice(NoticeCode::Banned, until as f64)
            );
        }
        Ok(ClaimOutcome::Full) => return Err(Refusal::new("server_full", 1, ErrorCode::ServerFull)),
        Err(_) => {
            log_error!(ctx.log, "presence claim not answered", { "userId": user });
            return Err(Refusal::internal());
        }
    };
    let claim = ClaimGuard::new(ctx.lobby.clone(), user, conn);
    match ctx.tokens.validate(hello.token.clone()).await {
        Ok(Some(_)) => Ok(Admitted { link, out, cmds, claim, active_game }),
        Ok(None) => Err(Refusal::new("unauthorized", 1, ErrorCode::Unauthorized)
            .with_notice(NoticeCode::SessionRevoked, 0.0)),
        Err(e) => {
            log_error!(ctx.log, "token validation failed", { "err": log::error_value("ValidateError", &e.to_string(), None, None) });
            Err(Refusal::internal())
        }
    }
}

/// Writes frames straight to the socket (before the writer task exists). False when the
/// connection is closing or the client does not read them within the close timeout.
async fn write_direct(ctx: &ConnContext, writer: &mut WsWriter, frames: &[Bytes]) -> bool {
    if frames.is_empty() {
        return true;
    }
    let refs: Vec<&[u8]> = frames.iter().map(|f| &f[..]).collect();
    matches!(tokio::time::timeout(ctx.settings.close_timeout, writer.send_batch(&refs)).await, Ok(Ok(())))
}

/// Refuses the connection: its `Notice` if any, the fatal `Error`, then the close code of the
/// rule.
pub(crate) async fn refuse(ctx: &ConnContext, writer: &mut WsWriter, refusal: Refusal) {
    metrics::conn().hello.with(&[refusal.label]).inc();
    let mut out: Vec<Bytes> = refusal.notice.into_iter().collect();
    out.push(frames::error(refusal.r#ref, refusal.code, true, 0));
    write_direct(ctx, writer, &out).await;
    let code = close_code_for(refusal.code).unwrap_or(CLOSE_INTERNAL);
    let reason = if refusal.code == ErrorCode::ShuttingDown { "server shutting down" } else { "" };
    writer.close(code, reason);
}
