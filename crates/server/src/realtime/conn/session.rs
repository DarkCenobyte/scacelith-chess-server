//! The session of an authenticated connection, after its `Welcome` (see the module
//! documentation of [`super`]): the per-message rules and the heartbeat, in [`Session`], and the
//! task's loop, in [`run`].

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use scacelith_protocol::{
    ClientGesture, ClientMsg, ErrorCode, Message, MsgType, NoticeCode, ServerPing, ServerPong,
    close_code_for, decode_hello, peek_seq,
};
use serde_json::{Value, json};
use tokio::task::{JoinError, JoinHandle};

use super::hello::Welcomed;
use super::{CLIENT_PING_GAP_MS, ConnContext, Gauge, MAX_GAMES_PER_CONN, RTT_CAP_MS};
use crate::events::Anomaly;
use crate::ids::{self, GameId, UserId};
use crate::log_error;
use crate::matching::ColorPref;
use crate::net::ws::{CLOSE_GOING_AWAY, CLOSE_INTERNAL, CLOSE_NORMAL, WsEvent, WsInfo, WsReader, WsWriter};
use crate::realtime::drain::{DrainPhase, DrainWatch};
use crate::realtime::endpoint::{CLOSE_SLOW_CONSUMER, Endpoint};
use crate::realtime::frames;
use crate::realtime::link::{ConnCmd, ConnLink};
use crate::realtime::lobby::{LobbyMsg, LobbyRequest};
use crate::realtime::metrics::{self, ConnMetrics};
use crate::realtime::reads;

/// Length of a drop window (ms).
const DROP_WINDOW_MS: f64 = 10_000.0;
/// The first type byte of the server-to-client range (PROTOCOL.md "Encoding").
const SERVER_TYPES: u8 = 0x80;
/// Least gap between two `RateLimited` errors (ms).
const RATE_ERROR_GAP_MS: f64 = 1000.0;
/// Rounding slack of a bucket (a millionth of a message): a client pacing its messages exactly at
/// the rate (1000 / 60 ms apart...) refills a whole token per message despite the rounding of the
/// times, which otherwise accumulates while the bucket is empty.
const TOKEN_SLACK: f64 = 1e-6;

/// A lobby request waiting for its turn: its store reads, or ready to be posted.
enum Pending {
    Reading(JoinHandle<LobbyRequest>),
    Ready(LobbyRequest),
}

/// A token bucket with its drop window.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: f64,
    drops: u32,
    window_at: f64,
}

impl Bucket {
    fn new(tokens: f64, now: f64) -> Bucket {
        Bucket { tokens, at: now, drops: 0, window_at: now }
    }

    /// Takes a token; false (nothing taken) when the bucket holds less than one.
    fn take(&mut self, rate: f64, burst: f64, now: f64) -> bool {
        let t = (self.tokens + (now - self.at) * rate / 1000.0).min(burst);
        self.at = now;
        if t < 1.0 - TOKEN_SLACK {
            self.tokens = t;
            return false;
        }
        self.tokens = t - 1.0;
        true
    }

    /// Counts a drop in the current window; returns the drops of the window.
    fn dropped(&mut self, now: f64) -> u32 {
        if now - self.window_at > DROP_WINDOW_MS {
            self.window_at = now;
            self.drops = 0;
        }
        self.drops += 1;
        self.drops
    }
}

/// The state of an authenticated connection (the former router's `ConnCtx`).
pub(crate) struct Session {
    ctx: Arc<ConnContext>,
    m: &'static ConnMetrics,
    link: Arc<ConnLink>,
    ep: Endpoint,
    info: WsInfo,
    user: UserId,
    last_seq: u32,
    bad_seq_reported: bool,
    msgs: Bucket,
    gestures: Bucket,
    last_rate_error_at: Option<f64>,
    last_client_ping_at: Option<f64>,
    ping_nonce: u32,
    /// When the unanswered `Ping` was sent.
    ping_sent_at: Option<f64>,
    next_ping_at: f64,
    /// Smoothed round trip (ms), 0 before the first sample.
    rtt: f64,
    /// Games attached, the most recent last.
    games: VecDeque<GameId>,
    /// Lobby requests in order of arrival, posted in that order once their reads are done.
    lobby: VecDeque<(u32, Pending)>,
}

impl Session {
    pub(crate) fn new(ctx: Arc<ConnContext>, link: Arc<ConnLink>, info: WsInfo) -> Session {
        let now = ctx.clock.mono_ms();
        let s = &ctx.settings;
        let gesture_tokens = if s.gesture_rate > 0.0 { s.gesture_burst } else { 0.0 };
        Session {
            m: metrics::conn(),
            ep: link.endpoint().clone(),
            user: link.user_id(),
            last_seq: 1,
            bad_seq_reported: false,
            msgs: Bucket::new(s.msg_burst, now),
            gestures: Bucket::new(gesture_tokens, now),
            last_rate_error_at: None,
            last_client_ping_at: None,
            ping_nonce: 0,
            ping_sent_at: None,
            next_ping_at: info.opened_at_ms() + s.heartbeat_interval_ms / 2.0,
            rtt: 0.0,
            games: VecDeque::new(),
            lobby: VecDeque::new(),
            link,
            info,
            ctx,
        }
    }

    fn send(&self, frame: Bytes) {
        self.ep.send(frame);
    }

    fn error(&self, r#ref: u32, code: ErrorCode, game: GameId) {
        self.send(frames::error(r#ref, code, false, game));
    }

    /// A fatal `Error`, the connection's last message, then its close code.
    fn fatal(&self, r#ref: u32, code: ErrorCode) {
        let close = close_code_for(code).unwrap_or(CLOSE_INTERNAL);
        self.ep.kick(&[frames::error(r#ref, code, true, 0)], close, "");
    }

    fn anomaly(&self, kind: &'static str, detail: Value, game: GameId) {
        self.m.anomalies.with(&[kind]).inc();
        if self.user != 0 {
            self.ctx.anomalies.record(Anomaly { user: self.user, game, kind, detail, pos_matched: false });
        }
    }

    /// The game attached last (0: none).
    fn current_game(&self) -> GameId {
        self.games.back().copied().unwrap_or(0)
    }

    /// One message of the client, as read.
    pub(crate) fn on_message(&mut self, buf: Bytes) {
        if !self.ep.is_open() {
            return;
        }
        let now = self.ctx.clock.mono_ms();
        let type_byte = buf.first().copied().unwrap_or(0);
        if type_byte == MsgType::ClientGesture.to_u8() {
            self.gesture(buf, now);
            return;
        }
        let s = &self.ctx.settings;
        if !self.msgs.take(s.msg_rate, s.msg_burst, now) {
            self.rate_limited(&buf, now);
            return;
        }
        // A type byte of the server's range (0x80-0xFF, assigned or not): no client sends one.
        if type_byte >= SERVER_TYPES {
            self.forged(type_byte);
            return;
        }
        // A Hello is read as at the handshake: a later minor's appended fields are ignored.
        let decoded = if type_byte == MsgType::Hello.to_u8() {
            decode_hello(&buf).map(ClientMsg::Hello)
        } else {
            ClientMsg::decode(&buf)
        };
        let msg = match decoded {
            Ok(msg) => msg,
            Err(e) => {
                self.anomaly("malformed", json!({ "reason": e.to_string(), "type": type_byte }), 0);
                self.fatal(peek_seq(&buf).unwrap_or(0), ErrorCode::Malformed);
                return;
            }
        };
        let seq = msg.seq();
        if seq != self.last_seq.wrapping_add(1) {
            self.bad_seq(seq);
            return;
        }
        self.last_seq = seq;
        self.m.count_in(type_byte);
        match msg {
            ClientMsg::Ping(p) => {
                if self.last_client_ping_at.is_none_or(|at| now - at >= CLIENT_PING_GAP_MS) {
                    self.last_client_ping_at = Some(now);
                    let server_time = self.ctx.clock.mono_ms();
                    if let Some(frame) = frames::encode(&ServerPong { nonce: p.nonce, server_time }) {
                        self.send(frame);
                    }
                } else {
                    self.m.drop_ping.inc();
                }
            }
            ClientMsg::Pong(p) => self.on_pong(p.nonce),
            ClientMsg::Hello(_) => self.error(seq, ErrorCode::ProtocolViolation, 0),
            ClientMsg::QueueJoin(q) => self.queue_join(seq, q.category, q.rated),
            ClientMsg::QueueLeave(_) => self.lobby_request(seq, Pending::Ready(LobbyRequest::QueueLeave)),
            ClientMsg::ChallengeCreate(c) => {
                self.challenge_create(seq, c.target, c.base_sec, c.inc_sec, c.rated, c.color.to_u8());
            }
            ClientMsg::ChallengeAccept(c) => {
                self.lobby_request(seq, Pending::Ready(LobbyRequest::ChallengeAccept { id: c.id }));
            }
            ClientMsg::ChallengeDecline(c) => {
                self.lobby_request(seq, Pending::Ready(LobbyRequest::ChallengeDecline { id: c.id }));
            }
            ClientMsg::ChallengeCancel(c) => {
                self.lobby_request(seq, Pending::Ready(LobbyRequest::ChallengeCancel { id: c.id }));
            }
            ClientMsg::ChallengeJoinCode(c) => {
                self.lobby_request(seq, Pending::Ready(LobbyRequest::ChallengeJoinCode { code: c.code }));
            }
            ClientMsg::Move(m) => self.game(seq, m.game, ClientMsg::Move(m)),
            ClientMsg::Resign(m) => self.game(seq, m.game, ClientMsg::Resign(m)),
            ClientMsg::DrawOffer(m) => self.game(seq, m.game, ClientMsg::DrawOffer(m)),
            ClientMsg::DrawAnswer(m) => self.game(seq, m.game, ClientMsg::DrawAnswer(m)),
            ClientMsg::DrawClaim(m) => self.game(seq, m.game, ClientMsg::DrawClaim(m)),
            ClientMsg::Abort(m) => self.game(seq, m.game, ClientMsg::Abort(m)),
            ClientMsg::Resync(m) => self.game(seq, m.game, ClientMsg::Resync(m)),
            ClientMsg::Rematch(m) => self.game(seq, m.game, ClientMsg::Rematch(m)),
            // Handled before the message bucket.
            ClientMsg::Gesture(_) => {}
        }
    }

    /// A gesture: its own bucket, then the checks of any message, then the relay.
    fn gesture(&mut self, buf: Bytes, now: f64) {
        let s = &self.ctx.settings;
        if !self.gestures.take(s.gesture_rate, s.gesture_burst, now) {
            self.m.g_drop_rate.inc();
            let seq = peek_seq(&buf).unwrap_or(0);
            if seq == self.last_seq.wrapping_add(1) {
                // The client counted it: stay in step.
                self.last_seq = seq;
            }
            let drops = self.gestures.dropped(now);
            if drops > s.gesture_flood_drops {
                self.anomaly("flood", json!({ "gestureDrops": drops }), 0);
                self.fatal(seq, ErrorCode::Flood);
            }
            return;
        }
        let g = match ClientGesture::decode(&buf) {
            Ok(g) => g,
            Err(e) => {
                let detail = json!({ "reason": e.to_string(), "type": MsgType::ClientGesture.to_u8() });
                self.anomaly("malformed", detail, 0);
                self.fatal(peek_seq(&buf).unwrap_or(0), ErrorCode::Malformed);
                return;
            }
        };
        if g.seq != self.last_seq.wrapping_add(1) {
            self.bad_seq(g.seq);
            return;
        }
        self.last_seq = g.seq;
        self.m.count_in(MsgType::ClientGesture.to_u8());
        // Only for a game of the connection (attached at its start or at the reconnection).
        if !self.games.contains(&g.game) {
            self.m.g_drop_not_attached.inc();
            return;
        }
        if !self.ctx.hosts.gesture(g.game, self.user, buf) {
            self.m.g_drop_no_game.inc();
        }
    }

    fn rate_limited(&mut self, buf: &[u8], now: f64) {
        self.m.drop_rate.inc();
        let seq = peek_seq(buf).unwrap_or(0);
        if seq == self.last_seq.wrapping_add(1) {
            // The client counted it: stay in step.
            self.last_seq = seq;
        }
        let drops = self.msgs.dropped(now);
        if drops > self.ctx.settings.flood_drops {
            self.anomaly("flood", json!({ "drops": drops }), 0);
            self.fatal(seq, ErrorCode::Flood);
            return;
        }
        if self.last_rate_error_at.is_none_or(|at| now - at >= RATE_ERROR_GAP_MS) {
            self.last_rate_error_at = Some(now);
            self.error(seq, ErrorCode::RateLimited, 0);
        }
    }

    fn bad_seq(&mut self, seq: u32) {
        self.m.drop_seq.inc();
        if !self.bad_seq_reported {
            self.bad_seq_reported = true;
            let expected = self.last_seq.wrapping_add(1);
            self.anomaly("bad_seq", json!({ "expected": expected, "got": seq }), 0);
        }
        if seq > self.last_seq {
            self.last_seq = seq;
        }
    }

    /// A type byte of the server's range from the client: a forgery, sanctioned as a certain
    /// cheat when `AUTO_SANCTION_CERTAIN_CHEATS` is on.
    fn forged(&mut self, type_byte: u8) {
        let game = self.current_game();
        self.anomaly("forged_type", json!({ "type": type_byte }), game);
        if !self.ctx.settings.auto_sanction {
            self.fatal(0, ErrorCode::ProtocolViolation);
            return;
        }
        // First: the ban holds before the forfeit or the close can reach the client.
        self.ctx.anomalies.sanction_certain(self.user, game, "forged_type", self.ep.conn_id());
        let mut shards = Vec::new();
        for &g in &self.games {
            let shard = ids::shard_of(g);
            if !shards.contains(&shard) {
                shards.push(shard);
                self.ctx.hosts.forfeit_user(g, self.user);
            }
        }
        self.fatal(0, ErrorCode::CheatDetected);
    }

    fn on_pong(&mut self, nonce: u32) {
        let Some(sent_at) = self.ping_sent_at else { return };
        if nonce != self.ping_nonce {
            return;
        }
        self.ping_sent_at = None;
        // A stall since the ping delayed the pong's reading, not the network.
        if self.ctx.hosts.stall_during(sent_at) {
            return;
        }
        let rtt = self.ctx.clock.mono_ms() - sent_at;
        self.m.rtt.observe(rtt);
        let sample = rtt.min(RTT_CAP_MS);
        self.rtt = if self.rtt > 0.0 { (self.rtt * 0.8 + sample * 0.2).min(RTT_CAP_MS) } else { sample };
        let ms = self.rtt.round() as u32;
        self.ep.set_rtt_ms(ms);
        for &g in &self.games {
            self.ctx.hosts.rtt(g, self.user, ms);
        }
    }

    /// The heartbeat: closes a silent connection (`None`), pings when it is time, and returns when
    /// to come back (monotonic ms).
    pub(crate) fn heartbeat(&mut self) -> Option<f64> {
        let s = &self.ctx.settings;
        let now = self.ctx.clock.mono_ms();
        let last_recv = self.info.last_recv_ms();
        if now - last_recv >= s.heartbeat_timeout_ms {
            self.ep.close(CLOSE_GOING_AWAY, "timeout");
            self.info.close(CLOSE_GOING_AWAY, "timeout");
            return None;
        }
        if now >= self.next_ping_at {
            self.next_ping_at = now + s.heartbeat_interval_ms;
            self.ping_nonce = self.ping_nonce.wrapping_add(1).max(1);
            self.ping_sent_at = Some(now);
            if let Some(frame) = frames::encode(&ServerPing { nonce: self.ping_nonce, server_time: now }) {
                self.send(frame);
            }
        }
        Some(self.next_ping_at.min(last_recv + s.heartbeat_timeout_ms))
    }

    /// When the heartbeat is due first (monotonic ms).
    pub(crate) fn first_heartbeat(&self) -> f64 {
        self.next_ping_at.min(self.info.last_recv_ms() + self.ctx.settings.heartbeat_timeout_ms)
    }

    fn queue_join(&mut self, seq: u32, category: String, rated: bool) {
        if self.ctx.categories.parse(&category).is_none() {
            self.error(seq, ErrorCode::InvalidCategory, 0);
            return;
        }
        // Joining a queue ends the rematch window of the last game.
        let last = self.current_game();
        if last != 0 {
            self.ctx.hosts.decline_rematch(last, self.user);
        }
        if !self.link.begin_request() {
            self.error(seq, ErrorCode::RateLimited, 0);
            return;
        }
        let ctx = self.ctx.clone();
        let user = self.user;
        let reads = tokio::spawn(async move {
            let (rating, provisional) =
                reads::rating_of(&ctx.store, &ctx.config, user, &category, &ctx.log).await;
            let ban = reads::stored_ban(&ctx.store, user, ctx.clock.wall_ms(), &ctx.log).await;
            let cooldown =
                if rated { reads::stored_cooldown(&ctx.store, user, &ctx.log).await } else { None };
            LobbyRequest::QueueJoin { category, rated, rating, provisional, ban, cooldown }
        });
        self.push_request(seq, Pending::Reading(reads));
    }

    fn challenge_create(
        &mut self,
        seq: u32,
        target: String,
        base_sec: u16,
        inc_sec: u8,
        rated: bool,
        color: u8,
    ) {
        if !self.link.begin_request() {
            self.error(seq, ErrorCode::RateLimited, 0);
            return;
        }
        let category = self
            .ctx
            .categories
            .category_of(i64::from(base_sec) * 1000, i64::from(inc_sec) * 1000)
            .to_string();
        let ctx = self.ctx.clone();
        let user = self.user;
        let reads = tokio::spawn(async move {
            let (rating, provisional) =
                reads::rating_of(&ctx.store, &ctx.config, user, &category, &ctx.log).await;
            let ban = reads::stored_ban(&ctx.store, user, ctx.clock.wall_ms(), &ctx.log).await;
            let color = ColorPref::from_u8(color);
            LobbyRequest::ChallengeCreate {
                target,
                base_sec,
                inc_sec,
                rated,
                color,
                rating,
                provisional,
                ban,
            }
        });
        self.push_request(seq, Pending::Reading(reads));
    }

    /// A lobby request that needs no read.
    fn lobby_request(&mut self, seq: u32, pending: Pending) {
        if !self.link.begin_request() {
            self.error(seq, ErrorCode::RateLimited, 0);
            return;
        }
        self.push_request(seq, pending);
    }

    fn push_request(&mut self, seq: u32, pending: Pending) {
        self.lobby.push_back((seq, pending));
        self.post_ready();
    }

    /// Posts the requests at the head of the line whose reads are done.
    fn post_ready(&mut self) {
        while matches!(self.lobby.front(), Some((_, Pending::Ready(_)))) {
            if let Some((seq, Pending::Ready(req))) = self.lobby.pop_front() {
                self.ctx.lobby.post(LobbyMsg::Request { link: self.link.clone(), seq, req });
            }
        }
    }

    /// Whether the request at the head of the line waits for its reads.
    pub(crate) fn reading(&self) -> bool {
        matches!(self.lobby.front(), Some((_, Pending::Reading(_))))
    }

    /// The reads of the request at the head of the line are done.
    pub(crate) fn read_done(&mut self, result: Result<LobbyRequest, JoinError>) {
        let Some((seq, pending)) = self.lobby.front_mut() else { return };
        match result {
            Ok(req) => *pending = Pending::Ready(req),
            Err(e) => {
                log_error!(self.ctx.log, "lobby request reads failed", { "err": e.to_string(), "userId": self.user });
                let seq = *seq;
                self.lobby.pop_front();
                self.link.end_request();
                self.error(seq, ErrorCode::Internal, 0);
            }
        }
        self.post_ready();
    }

    fn game(&mut self, seq: u32, game: GameId, msg: ClientMsg) {
        if !ids::is_game_id(game) {
            self.error(seq, ErrorCode::NotInGame, 0);
            return;
        }
        let recv_at = self.info.last_recv_ms();
        if !self.ctx.hosts.client(game, self.user, msg, self.ep.clone(), recv_at) {
            self.error(seq, ErrorCode::NotInGame, game);
        }
    }

    /// Binds the connection to a game (the game in progress at the `Welcome`, or a game the lobby
    /// created); the host sends the snapshot. The oldest of more than 8 games is detached.
    pub(crate) fn attach(&mut self, game: GameId) {
        if !ids::is_game_id(game) {
            return;
        }
        self.games.retain(|&g| g != game);
        self.games.push_back(game);
        if self.games.len() > MAX_GAMES_PER_CONN
            && let Some(oldest) = self.games.pop_front()
        {
            self.ctx.hosts.detach(oldest, self.user, self.ep.conn_id());
        }
        self.ctx.hosts.attach(game, self.user, self.ep.clone());
    }

    /// A command of the lobby.
    pub(crate) fn command(&mut self, cmd: ConnCmd) {
        match cmd {
            ConnCmd::Attach(game) => self.attach(game),
        }
    }

    /// A new phase of the drain.
    pub(crate) fn drain(&mut self, phase: DrainPhase) {
        match phase {
            DrainPhase::Running => {}
            DrainPhase::Grace { grace_ms } => {
                self.send(frames::notice(NoticeCode::ServerShutdown, grace_ms as f64))
            }
            DrainPhase::Closing => {
                let bye = frames::error(0, ErrorCode::ShuttingDown, true, 0);
                let close = close_code_for(ErrorCode::ShuttingDown).unwrap_or(CLOSE_INTERNAL);
                self.ep.kick(&[bye], close, "server shutting down");
            }
        }
    }

    /// The connection closed: its games are detached.
    pub(crate) fn detach_all(&mut self) {
        for g in std::mem::take(&mut self.games) {
            self.ctx.hosts.detach(g, self.user, self.ep.conn_id());
        }
    }
}

/// The reads of the request at the head of the line (never resolves when it needs none).
async fn head_reads(lobby: &mut VecDeque<(u32, Pending)>) -> Result<LobbyRequest, JoinError> {
    match lobby.front_mut() {
        Some((_, Pending::Reading(reads))) => reads.await,
        _ => std::future::pending().await,
    }
}

/// Runs the session of a welcomed connection until it ends.
pub(crate) async fn run(
    ctx: Arc<ConnContext>,
    mut reader: WsReader,
    writer: WsWriter,
    info: WsInfo,
    mut drain: DrainWatch,
    welcomed: Welcomed,
) {
    let Welcomed { link, out, mut cmds, claim, active_game, pending } = welcomed;
    let _player = Gauge::up(&metrics::conn().players);
    let mut writer_task = tokio::spawn(super::writer::run(writer, out.clone()));
    let mut s = Session::new(ctx.clone(), link, info.clone());
    if active_game != 0 {
        s.attach(active_game);
    }
    for buf in pending {
        s.on_message(buf);
    }
    // The drain may have moved on during the Hello's last steps.
    let phase = drain.current();
    s.drain(phase);
    let beat = tokio::time::sleep_until(ctx.instant_at(s.first_heartbeat()));
    tokio::pin!(beat);
    let mut reader_done = false;
    loop {
        tokio::select! {
            biased;
            () = out.closed() => break,
            phase = drain.changed() => s.drain(phase),
            Some(cmd) = cmds.recv() => s.command(cmd),
            () = &mut beat => match s.heartbeat() {
                Some(at) => beat.as_mut().reset(ctx.instant_at(at)),
                None => break,
            },
            result = head_reads(&mut s.lobby), if s.reading() => s.read_done(result),
            ev = reader.next() => match ev {
                WsEvent::Message(buf) => s.on_message(buf),
                WsEvent::Closed(_) => {
                    reader_done = true;
                    break;
                }
            },
        }
    }

    // The players' games and presence learn it at once; the socket gets its last frames.
    s.detach_all();
    drop(claim);
    if reader_done {
        out.close(CLOSE_NORMAL, "");
    }
    // The writer task is never aborted: a write cut short would leave half a frame before the
    // close frame. When it is stuck on a client that does not read, this task starts the close,
    // which fails the pending write once the socket is dropped (the close timeout later).
    let writer_done = match out.close_request() {
        Some(close) if close.code == CLOSE_SLOW_CONSUMER => {
            info.close(close.code, &close.reason);
            false
        }
        Some(close) => match tokio::time::timeout(ctx.settings.close_timeout, &mut writer_task).await {
            Ok(_) => true,
            Err(_) => {
                info.close(close.code, &close.reason);
                false
            }
        },
        None => {
            out.close(CLOSE_INTERNAL, "");
            info.close(CLOSE_INTERNAL, "");
            false
        }
    };
    if !reader_done {
        super::linger(&mut reader).await;
    }
    if !writer_done {
        let _ = writer_task.await;
    }
}
