//! The game part: a server with two shards, proof of work on registration and no GIFs; a bot that
//! queues in rated 3+2 and plays random legal moves; then the C++ test `net_live_server_game`
//! (registration with the proof of work, sign-in, queue, 12 plies against the bot, resignation,
//! rating update) and `net_live_account_server_settings` (an e-mail change refused for the bot's
//! address, then applied at once on a server without e-mail confirmation, and the GIFs turned
//! off).

use std::time::Duration;

use scacelith_client::bot::{Bot, BotConfig};
use scacelith_client::{ApiClient, ConnectOptions, Connection};
use scacelith_server::security::pow::solve_pow;
use serde_json::{Value, json};

use crate::support::web::call;
use crate::support::{PASSWORD, TestServer, sign_in};
use crate::{Ctx, run_cpp};

/// The C++ player of the part (registered by the C++ test).
const CPP_USER: &str = "cppplayer";
const CPP_PASSWORD: &str = "live check password 1234";
/// The bot's account; its address is the one the settings test finds taken.
const BOT: &str = "livebot";

/// Registers `name` on a server that asks for a proof of work: the first request gets the
/// challenge (428 `pow_required`), the second carries its solution.
async fn register_with_pow(api: &ApiClient, name: &str) -> Result<(), String> {
    let mut body = json!({"username": name, "email": format!("{name}@example.org"), "password": PASSWORD});
    let (res, answer) = call(api, "POST", "/auth/register", None, Some(body.clone())).await;
    let (res, answer) = if res.status == 428 {
        let challenge = answer["pow"]["challenge"].as_str().unwrap_or_default().to_owned();
        let bits = answer["pow"]["bits"].as_u64().unwrap_or(0) as u32;
        let solving = challenge.clone();
        let nonce = tokio::task::spawn_blocking(move || solve_pow(&solving, bits))
            .await
            .map_err(|e| format!("proof of work: {e}"))?;
        body["pow"] = json!({"challenge": challenge, "nonce": nonce});
        call(api, "POST", "/auth/register", None, Some(body)).await
    } else {
        (res, answer)
    };
    if (200..300).contains(&res.status) {
        Ok(())
    } else {
        Err(format!("register {name}: {} {answer}", res.status))
    }
}

pub(crate) async fn run(ctx: &Ctx) -> i32 {
    let srv =
        ctx.server().workers(2).env("POW_REGISTER_BITS", "12").env("GIF_ENABLED", "false").start().await;
    let mut srv = srv;
    let pin = srv.pin();
    println!("[game] server on {}:{} (API + WSS), certificate SHA-256 {pin}", ctx.host, srv.addr.port());
    let code = play(ctx, &srv, &pin).await;
    srv.stop().await;
    code
}

async fn play(ctx: &Ctx, srv: &TestServer, pin: &str) -> i32 {
    if let Err(e) = register_with_pow(&srv.api(), BOT).await {
        eprintln!("[game] {e}");
        return 1;
    }
    let bot_acc = sign_in(srv, BOT, None).await;
    let conn = match Connection::connect(&srv.endpoint(), &bot_acc.token, &ConnectOptions::default()).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("[game] the bot cannot connect: {e}");
            return 1;
        }
    };
    // The bot waits in the queue while the C++ test registers and signs in.
    let cfg = BotConfig {
        move_delay: Duration::from_millis(350),
        move_jitter: 0.4,
        timeout: Duration::from_secs(300),
        ..BotConfig::default()
    };
    let bot = tokio::spawn(async move {
        let mut bot = Bot::new(conn, cfg);
        let game = bot.join_queue("3+2", true).await?;
        println!("[game] bot: game {}, playing {:?}", game.game(), game.you());
        let id = game.game();
        let result = bot.play(game).await?;
        println!(
            "[game] bot: game over, status {:?} reason {:?}, {} plies",
            result.end.status, result.end.reason, result.plies
        );
        Ok::<_, scacelith_client::ClientError>(id)
    });

    let live = format!("{}:{}:{pin}:{CPP_USER}:{CPP_PASSWORD}", ctx.host, srv.addr.port());
    let mut code = run_cpp(ctx, "net_live_server_game", &[("SCACELITH_NET_LIVE", &live)]).await;
    let game_id = match tokio::time::timeout(Duration::from_secs(30), bot).await {
        Ok(Ok(Ok(id))) => Some(id),
        Ok(Ok(Err(e))) => {
            eprintln!("[game] bot: {e}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("[game] bot: {e}");
            None
        }
        Err(_) => {
            eprintln!("[game] bot: the game did not end");
            None
        }
    };
    println!("[game] C++ test exit code {code}");
    if code != 0 {
        return code;
    }
    let Some(game_id) = game_id else { return 1 };

    // This server has no e-mail confirmation (the e-mail change applies at once) and no GIFs.
    let settings = format!(
        "{}:{}:{pin}:{CPP_USER}:{CPP_PASSWORD}:{BOT}@example.org:{game_id}",
        ctx.host,
        srv.addr.port()
    );
    code =
        run_cpp(ctx, "net_live_account_server_settings", &[("SCACELITH_NET_LIVE_SETTINGS", &settings)]).await;
    let subject = |m: &Value| m["subject"].as_str().unwrap_or_default().to_owned();
    let notice = srv
        .mails_to(&format!("{CPP_USER}@example.org"))
        .into_iter()
        .find(|m| subject(m).contains("was changed"));
    match &notice {
        Some(m) => println!("[game] notice to the former address: \"{}\"", subject(m)),
        None => println!("[game] notice to the former address: MISSING"),
    }
    if notice.is_none() && code == 0 {
        code = 1;
    }
    match srv.mails_to(&format!("{BOT}@example.org")).first() {
        Some(m) => println!("[game] notice to the owner of the address in use: \"{}\"", subject(m)),
        None => println!("[game] notice to the owner of the address in use: none"),
    }
    code
}
