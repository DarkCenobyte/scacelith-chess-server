//! The account part: the account API (docs/API.md) end to end. A server with one shard (so that
//! every request reaches the same GIF cache) and e-mail confirmation on, the mails read from its
//! log. The harness registers and confirms three accounts, plays games through the realtime
//! protocol as the C++ player's account against a rival (rated win and loss, casual draw by
//! agreement, a custom time control won by checkmate, a promotion, an aborted game) and one game
//! between two other players. Then the C++ test `net_live_account_api` signs in with the C++
//! client and calls the account API, driving the harness through the control server.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use scacelith_protocol::{
    Abort, ChallengeDecline, ColorPref, EndReason, ErrorCode, GameEventKind, GameSnapshot, GameStatus,
    ServerMsg,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::control::{Call, Control, Reply, Routes};
use crate::support::players::verified_account;
use crate::support::web::{call, link_path, page, query_param, text};
use crate::support::{PASSWORD, Player, Table, TestServer, WAIT, connect, metric_value};
use crate::{Ctx, fresh_totp, now_ms, run_cpp, wait_msg};

/// The C++ player's account (played by the harness here, then signed in by the C++ test).
const ACCOUNT_USER: &str = "cpp_account";
/// The client label of the harness's own sessions.
const HARNESS_LABEL: &str = "live harness";

pub(crate) async fn run(ctx: &Ctx) -> i32 {
    let mut srv = ctx
        .server()
        .workers(1)
        .env("REQUIRE_EMAIL_VERIFICATION", "true")
        .env("FIRST_MOVE_TIMEOUT_MS", "20000")
        .env("DB_COMMIT_MS", "20")
        .start()
        .await;
    let pin = srv.pin();
    println!("[account] server on {}:{} (API + WSS), certificate SHA-256 {pin}", ctx.host, srv.addr.port());
    let code = check(ctx, &srv, &pin).await;
    let errors = srv.logs.matching(|l| l["level"] == "error");
    if !errors.is_empty() {
        println!("[account] server errors logged:");
        for e in errors {
            println!("{e}");
        }
    }
    srv.stop().await;
    code
}

/// A verified player of the harness, connected.
async fn harness_player(srv: &TestServer, name: &str) -> Player {
    let acc = verified_account(srv, name, Some(HARNESS_LABEL)).await;
    let client = connect(srv, &acc.token).await.unwrap_or_else(|e| panic!("connect {name}: {e}"));
    Player { acc, client }
}

async fn check(ctx: &Ctx, srv: &TestServer, pin: &str) -> i32 {
    let mut me = harness_player(srv, ACCOUNT_USER).await;
    let mut rival = harness_player(srv, "rival_live").await;
    let mut third = harness_player(srv, "third_live").await;
    let (games, other) = play_games(&mut me, &mut rival, &mut third).await;
    // Every game committed before the C++ client reads the history.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (res, body) =
            call(&me.acc.api, "GET", "/account/games?limit=50", Some(&me.acc.token), None).await;
        if res.status == 200 && body["total"].as_u64() == Some(games.len() as u64) {
            break;
        }
        if Instant::now() > deadline {
            eprintln!("[account] history not committed: {body}");
            return 1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let renders: u32 = srv
        .env
        .iter()
        .find(|(k, _)| k == "GIF_USER_RENDERS_PER_MIN")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(4);
    let summary: Vec<String> = games
        .iter()
        .map(|g| format!("{} {}", g["id"].as_str().unwrap_or(""), g["outcome"].as_str().unwrap_or("")))
        .collect();
    println!("[account] games of {ACCOUNT_USER}: {}; other game {}", summary.join(", "), other["id"]);
    let state = json!({
        "user": ACCOUNT_USER, "password": PASSWORD, "email": me.acc.email, "userId": me.acc.user_id,
        "rival": rival.name(), "third": third.name(), "harnessLabel": HARNESS_LABEL,
        "games": games, "other": other, "gifUserRendersPerMin": renders,
    });
    let ctl = match Control::bind().await {
        Ok(ctl) => ctl,
        Err(e) => {
            eprintln!("[account] control server: {e}");
            return 1;
        }
    };
    let user_id = me.acc.user_id;
    let routes = AccountRoutes {
        srv,
        state,
        user_id,
        players: Mutex::new((me, rival)),
        totp_steps: Mutex::new(HashMap::new()),
    };
    let live = format!("{}:{}:{pin}:{}", ctx.host, srv.addr.port(), ctl.port);
    let env = [("SCACELITH_NET_LIVE_ACCOUNT", live.as_str())];
    let code = tokio::select! {
        code = run_cpp(ctx, "net_live_account_api", &env) => code,
        () = ctl.serve(&routes) => 1,
    };
    println!("[account] C++ test exit code {code}");
    let (mut me, mut rival) = routes.players.into_inner();
    me.client.close().await;
    rival.client.close().await;
    third.client.close().await;
    code
}

/// `a` challenges `b` (White asked for `a`) once neither is in a game any more (the players are
/// freed a moment after the end of their previous game), `b` accepts. Returns the game id.
async fn challenge(a: &mut Player, b: &mut Player, base_sec: u16, inc_sec: u8, rated: bool) -> u64 {
    for _ in 0..200 {
        let (ma, mb) = (a.client.mark(), b.client.mark());
        let seq = a.client.challenge(b.name(), base_sec, inc_sec, rated, ColorPref::White);
        let outcome = tokio::select! {
            id = b.client.try_wait_for(mb, WAIT, |m| match m {
                ServerMsg::ChallengeReceived(r) => Some(r.id),
                _ => None,
            }) => id.map(Ok),
            code = a.client.try_wait_for(ma, WAIT, |m| match m {
                ServerMsg::Error(e) if e.r#ref == seq => Some(e.code),
                _ => None,
            }) => code.map(Err),
        };
        match outcome {
            Some(Ok(id)) => {
                b.client.accept_challenge(id);
                let sa: GameSnapshot = wait_msg!(a.client, ma, ServerMsg::GameSnapshot(s) => s.clone());
                wait_msg!(b.client, mb, ServerMsg::GameSnapshot(s) if s.game == sa.game => ());
                return sa.game;
            }
            Some(Err(ErrorCode::AlreadyInGame | ErrorCode::UserUnavailable)) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("challenge {} -> {}: {other:?}", a.name(), b.name()),
        }
    }
    panic!("{} and {} stayed in a game", a.name(), b.name());
}

/// The end of game `id` as `p` sees it, then its commit (`GET /games/:id` answers).
async fn end_of(p: &mut Player, id: u64, since: usize) {
    wait_msg!(p.client, since, Duration::from_secs(20), ServerMsg::GameEnd(e) if e.game == id => ());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (res, _) = call(&p.acc.api, "GET", &format!("/games/{id}"), Some(&p.acc.token), None).await;
        if res.status == 200 {
            return;
        }
        assert!(Instant::now() < deadline, "game {id} not committed");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// What the C++ test expects of a game of the C++ player.
#[allow(clippy::too_many_arguments)]
fn note(
    id: u64,
    color: &str,
    outcome: &str,
    rated: bool,
    category: &str,
    plies: u32,
    result: &str,
    status: GameStatus,
    reason: EndReason,
) -> Value {
    println!("[account] game {id}: {ACCOUNT_USER} {color}, {outcome}");
    let termination = if status == GameStatus::Aborted { "unterminated" } else { "normal" };
    json!({
        "id": id.to_string(), "color": color, "outcome": outcome, "rated": rated, "category": category,
        "plies": plies, "result": result, "status": status.to_u8(), "reason": reason.to_u8(),
        "termination": termination,
    })
}

/// The games of the check: `me` is the C++ player's account (played here by the harness), `rival`
/// its opponent, `third` the other player of a game `me` is not in. Returns what the C++ test
/// expects of each game of `me` (newest last) and of the other game.
async fn play_games(me: &mut Player, rival: &mut Player, third: &mut Player) -> (Vec<Value>, Value) {
    let mut list = Vec::new();

    // 1. Rated 3+2, me White: the rival resigns.
    let g = challenge(me, rival, 180, 2, true).await;
    Table::new(g).play_all(me, rival, &["e2e4", "e7e5", "g1f3", "b8c6", "f1c4"]).await;
    let m = me.client.mark();
    rival.client.resign(g);
    end_of(me, g, m).await;
    list.push(note(g, "white", "win", true, "3+2", 5, "1-0", GameStatus::WhiteWins, EndReason::Resignation));

    // 2. Rated 3+2, me Black: I resign.
    let g = challenge(rival, me, 180, 2, true).await;
    Table::new(g).play_all(rival, me, &["e2e4", "c7c5", "g1f3"]).await;
    let m = me.client.mark();
    me.client.resign(g);
    end_of(me, g, m).await;
    list.push(note(g, "black", "loss", true, "3+2", 3, "1-0", GameStatus::WhiteWins, EndReason::Resignation));

    // 3. Casual 3+2, me White: a draw by agreement.
    let g = challenge(me, rival, 180, 2, false).await;
    Table::new(g).play_all(me, rival, &["d2d4", "d7d5", "c2c4", "e7e6"]).await;
    let (m, mr) = (me.client.mark(), rival.client.mark());
    me.client.offer_draw(g);
    wait_msg!(rival.client, mr, ServerMsg::GameEvent(e) if e.game == g && e.kind == GameEventKind::DrawOffered => ());
    rival.client.answer_draw(g, true);
    end_of(me, g, m).await;
    list.push(note(g, "white", "draw", false, "3+2", 4, "1/2-1/2", GameStatus::Draw, EndReason::Agreement));

    // 4. Casual 7+1 (not an official category: custom), me Black: fool's mate.
    let g = challenge(rival, me, 420, 1, false).await;
    let m = me.client.mark();
    Table::new(g).play_all(rival, me, &["f2f3", "e7e5", "g2g4", "d8h4"]).await;
    end_of(me, g, m).await;
    list.push(note(
        g,
        "black",
        "win",
        false,
        "custom",
        4,
        "0-1",
        GameStatus::BlackWins,
        EndReason::Checkmate,
    ));

    // 5. Casual 3+2, me White: a pawn promotes to a queen (b7xa8=Q), then the rival resigns.
    let g = challenge(me, rival, 180, 2, false).await;
    let moves = ["e2e4", "d7d5", "e4d5", "c7c6", "d5c6", "g8f6", "c6b7", "b8d7", "b7a8q"];
    Table::new(g).play_all(me, rival, &moves).await;
    let m = me.client.mark();
    rival.client.resign(g);
    end_of(me, g, m).await;
    let mut promo =
        note(g, "white", "win", false, "3+2", 9, "1-0", GameStatus::WhiteWins, EndReason::Resignation);
    promo["promotion"] = json!({"ply": 8, "uci": "b7a8q", "san": "bxa8=Q"});
    list.push(promo);

    // 6. Casual 3+2, me White: aborted before the first move.
    let g = challenge(me, rival, 180, 2, false).await;
    let m = me.client.mark();
    me.client.send(Abort { seq: 0, game: g });
    end_of(me, g, m).await;
    list.push(note(g, "white", "aborted", false, "3+2", 0, "*", GameStatus::Aborted, EndReason::Aborted));

    // Another players' game: the rival (White) against the third player, who resigns.
    let g = challenge(rival, third, 180, 2, false).await;
    Table::new(g).play_all(rival, third, &["e2e4", "e7e5"]).await;
    let m = rival.client.mark();
    third.client.resign(g);
    end_of(rival, g, m).await;
    let other = json!({"id": g.to_string(), "white": rival.name(), "black": third.name(), "plies": 2, "result": "1-0"});
    (list, other)
}

/// The control routes of `net_live_account_api`.
struct AccountRoutes<'a> {
    srv: &'a TestServer,
    state: Value,
    user_id: u32,
    /// The harness's connections of the C++ player's account and of the rival.
    players: Mutex<(Player, Player)>,
    totp_steps: Mutex<HashMap<String, i64>>,
}

impl AccountRoutes<'_> {
    /// The latest mail to `to` whose subject contains `subject`, once more than `after` of them
    /// were written (within 5 s), with their count.
    async fn mail(&self, to: &str, subject: &str, after: usize) -> Option<(Value, usize)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let all: Vec<Value> = self
                .srv
                .mails_to(to)
                .into_iter()
                .filter(|m| m["subject"].as_str().is_some_and(|s| s.contains(subject)))
                .collect();
            if all.len() > after {
                let n = all.len();
                return all.into_iter().last().map(|m| (m, n));
            }
            if Instant::now() > deadline {
                return None;
            }
            self.srv.logs.changed(Duration::from_millis(100)).await;
        }
    }

    /// Waits (5 s at most) until a security event `kind` of the C++ player is in the database.
    /// The server saves them in batches, a second after the first event of a batch (DESIGN.md
    /// section 7); the export that the C++ test asks for right after the e-mail change must hold
    /// the change and the wrong password before it (`reauth_failed`, saved in the same batch or an
    /// earlier one).
    async fn security_event_saved(&self, kind: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let db = self.srv.db();
        loop {
            let found = db
                .query_row(
                    "SELECT COUNT(*) FROM security_events WHERE user_id = ?1 AND kind = ?2",
                    rusqlite::params![self.user_id, kind],
                    |r| r.get::<_, i64>(0),
                )
                .is_ok_and(|n| n > 0);
            if found || Instant::now() > deadline {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The newest active session of the C++ player that this harness did not open expires now.
    fn expire_session(&self) -> Result<Value, String> {
        let db = self.srv.db();
        let now = now_ms();
        let row: Option<(i64, Option<String>)> = db
            .query_row(
                "SELECT id, client_label FROM sessions WHERE user_id = ?1 AND revoked_at IS NULL AND expires_at > ?2
                 AND (client_label IS NULL OR client_label NOT LIKE 'live harness%') ORDER BY id DESC LIMIT 1",
                rusqlite::params![self.user_id, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((id, label)) = row else { return Err("no_session".into()) };
        db.execute(
            "UPDATE sessions SET expires_at = ?1, idle_expires_at = ?1 WHERE id = ?2",
            rusqlite::params![now - 1000, id],
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({"sessionId": id, "clientLabel": label}))
    }

    /// Session `id` of the C++ player revoked from another device (a new harness sign-in).
    async fn revoke_session(&self, id: &str) -> Reply {
        let api = self.srv.api();
        let password = self.state["password"].as_str().unwrap_or_default();
        let login = json!({"login": ACCOUNT_USER, "password": password, "clientLabel": format!("{HARNESS_LABEL} (revoker)")});
        let (res, body) = call(&api, "POST", "/auth/login", None, Some(login)).await;
        let Some(token) = body["token"].as_str().map(str::to_owned).filter(|_| res.status == 200) else {
            return Reply::with_status(500, json!({"error": "login", "status": res.status, "body": body}));
        };
        let encoded = crate::support::web::encode(id);
        let (res, body) =
            call(&api, "DELETE", &format!("/auth/sessions/{encoded}"), Some(&token), None).await;
        let _ = call(&api, "POST", "/auth/logout", Some(&token), Some(json!({}))).await;
        Reply::ok(json!({"status": res.status, "body": body}))
    }

    /// The rival challenges the C++ player by name (the harness's own connection of that account
    /// is online): `delivered` (then declined) or the refusal's error code.
    async fn challenge(&self) -> Reply {
        let mut players = self.players.lock().await;
        let (me, rival) = &mut *players;
        let (mm, mr) = (me.client.mark(), rival.client.mark());
        let seq = rival.client.challenge(ACCOUNT_USER, 180, 2, false, ColorPref::White);
        let limit = Duration::from_secs(3);
        let received = |m: &ServerMsg| match m {
            ServerMsg::ChallengeReceived(r) => Some(r.id),
            _ => None,
        };
        let outcome = tokio::select! {
            id = me.client.try_wait_for(mm, limit, received) => id.map(Ok),
            answer = rival.client.try_wait_for(mr, limit, |m| match m {
                ServerMsg::Ack(a) if a.r#ref == seq => Some(None),
                ServerMsg::Error(e) if e.r#ref == seq => Some(Some(e.code)),
                _ => None,
            }) => answer.map(Err),
        };
        let delivered = match outcome {
            Some(Ok(id)) => Some(id),
            Some(Err(Some(code))) => return Reply::ok(json!({"result": code.name()})),
            // Acknowledged (or no answer yet): wait for the delivery a little longer.
            Some(Err(None)) | None => me.client.try_wait_for(mm, limit, received).await,
        };
        match delivered {
            Some(id) => {
                me.client.send(ChallengeDecline { seq: 0, id });
                Reply::ok(json!({"result": "delivered"}))
            }
            None => Reply::ok(json!({"result": "acked_not_delivered"})),
        }
    }
}

impl Routes for AccountRoutes<'_> {
    async fn answer(&self, c: &Call) -> Reply {
        match (c.method.as_str(), c.path.as_str()) {
            ("GET", "/state") => Reply::ok(self.state.clone()),
            ("GET", "/mail") => {
                match self.mail(c.q("to"), c.q("subject"), c.q("after").parse().unwrap_or(0)).await {
                    Some((m, count)) => Reply::ok(
                        json!({"to": m["to"], "subject": m["subject"], "text": m["text"], "count": count}),
                    ),
                    None => Reply::error(404, "no_mail"),
                }
            }
            // The confirmation link mailed to ?to=, opened (GET) and its button pressed (POST),
            // then the change saved among the security events (below).
            ("POST", "/confirm-email-change") => {
                let Some((mail, _)) = self.mail(c.q("to"), "Confirm your new e-mail address", 0).await else {
                    return Reply::error(404, "no_mail");
                };
                let lp = link_path(mail["text"].as_str().unwrap_or_default());
                let shown = page(self.srv, "GET", &lp, None).await;
                let token = query_param(&lp, "token").unwrap_or_default();
                let done = page(self.srv, "POST", "/confirm-email-change", Some(&[("token", &token)])).await;
                let saved = self.security_event_saved("email_changed").await;
                Reply::ok(json!({
                    "link": lp, "getStatus": shown.status, "status": done.status,
                    "changed": text(&done).contains("E-mail address changed"), "eventSaved": saved,
                }))
            }
            ("POST", "/expire-session") => match self.expire_session() {
                Ok(body) => Reply::ok(body),
                Err(e) if e == "no_session" => Reply::error(404, "no_session"),
                Err(e) => Reply::with_status(500, json!({"error": "harness", "message": e})),
            },
            ("POST", "/revoke-session") => self.revoke_session(c.q("id")).await,
            ("POST", "/challenge") => self.challenge().await,
            ("GET", "/totp") => Reply::ok(fresh_totp(&mut *self.totp_steps.lock().await, c.q("secret"))),
            ("GET", "/metric") => {
                Reply::ok(json!({"value": metric_value(&self.srv.metrics().await, c.q("name"))}))
            }
            _ => Reply::error(404, "no_route"),
        }
    }
}
