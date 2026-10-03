//! The account API end to end on the real server (TLS, SQLite), with e-mail confirmation on and
//! the mails read from the log transport: register and confirm, log in, play a rated game to its
//! end, the player's history (`/account/games`), the caller-aware game record (`/games/:id`: you,
//! reportable), its PGN, an e-mail change through its link, the data export, the deletion of the
//! account and a password reset (both close the live connection of the revoked sessions). Then
//! the default port: a server started without the right to bind 443 logs how to fix it and exits
//! non-zero.
//!
//! Port of the Node.js `test/integration/account-api.test.js`. The Rust server is one process:
//! the failed bind ends the process itself (exit status 1), where the Node.js primary logged the
//! exit of its worker.

#[macro_use]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use scacelith_client::ApiClient;
use scacelith_client::http::Response;
use scacelith_protocol::{NoticeCode, ServerMsg, close};
use serde_json::{Value, json};
use support::server::{Logs, SERVER_BIN};
use support::web::{link_path, page, query_param, text};
use support::*;
use tokio::process::Command;

/// A request through the API client: the answer whatever its status, and its JSON body (`Null`
/// when it has none).
async fn call(
    api: &ApiClient,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (Response, Value) {
    let res = api
        .request(method, path, token, body.as_ref())
        .await
        .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
    let json = serde_json::from_slice(&res.body).unwrap_or(Value::Null);
    (res, json)
}

/// Registers `name`, confirms the address with the link of the mail, logs in (client label
/// "integration test") and connects.
async fn verified_player(srv: &TestServer, name: &str) -> Player {
    let api = srv.api();
    let email = format!("{name}@example.org");
    let body = json!({"username": name, "email": email, "password": PASSWORD});
    let (res, answer) = call(&api, "POST", "/auth/register", None, Some(body)).await;
    assert_eq!((res.status, answer), (202, json!({"status": "verification_sent"})));
    // No account before the link is used: the sign-in fails as for an unknown name, and the
    // database has no row for it.
    let login = json!({"login": name, "password": PASSWORD});
    let (res, answer) = call(&api, "POST", "/auth/login", None, Some(login.clone())).await;
    assert_eq!((res.status, answer["error"].as_str()), (401, Some("invalid_credentials")));
    let rows: i64 = srv
        .db()
        .query_row("SELECT COUNT(*) FROM users WHERE username = ?1", [name], |r| r.get(0))
        .expect("users counted");
    assert_eq!(rows, 0);

    let mail = srv.mail_to(&email, "Confirm your e-mail address for", 0).await;
    let link = link_path(mail["text"].as_str().expect("the mail's text"));
    let token = query_param(&link, "token").expect("the link's token");
    assert_eq!(page(srv, "GET", &link, None).await.status, 200);
    let done = page(srv, "POST", "/verify-email", Some(&[("token", &token)])).await;
    assert_eq!(done.status, 200, "{}", text(&done));

    let acc = sign_in(srv, name, Some("integration test")).await;
    assert_eq!(acc.email, email);
    let client = connect(srv, &acc.token).await.expect("connected");
    Player { acc, client }
}

/// The close code of the player's connection, once it got `Notice{SessionRevoked}` and closed.
async fn closed_as_revoked(p: &mut Player) -> u16 {
    let limit = Duration::from_secs(10);
    wait_msg!(p.client, 0, limit, ServerMsg::Notice(n) if n.code == NoticeCode::SessionRevoked => ());
    p.client.closed(limit).await.code
}

#[tokio::test]
async fn register_play_history_game_record_pgn_email_change_export_deletion() {
    let srv = TestServer::options()
        .env("REQUIRE_EMAIL_VERIFICATION", "true")
        .env("FIRST_MOVE_TIMEOUT_MS", "20000")
        .env("DB_COMMIT_MS", "20")
        .start()
        .await;
    let (alice, bob) = tokio::join!(verified_player(&srv, "alice_api"), verified_player(&srv, "bob_api"));
    let (mut alice, mut bob) = (alice, bob);

    // A rated 3+2 game that Bob (Black) resigns.
    let id = challenge_game(&mut alice, &mut bob, 180, 2, true).await;
    let mut table = Table::new(id);
    table.play_all(&mut alice, &mut bob, &["e2e4", "e7e5", "g1f3", "b8c6", "f1c4"]).await;
    let m = alice.client.mark();
    bob.client.resign(id);
    wait_msg!(alice.client, m, ServerMsg::GameEnd(e) if e.game == id => ());

    // The history, once the game is committed.
    let (a_api, a_tok) = (&alice.acc.api, alice.acc.token.clone());
    let (b_api, b_tok) = (&bob.acc.api, bob.acc.token.clone());
    let hist = eventually(Duration::from_secs(10), "the committed game in the history", || async {
        let (res, body) = call(a_api, "GET", "/account/games", Some(&a_tok), None).await;
        (res.status == 200 && body["total"] == 1).then_some(body)
    })
    .await;
    assert_eq!(hist["next"], Value::Null);
    let sum = &hist["games"][0];
    assert_eq!(sum["id"].to_string().trim_matches('"'), id.to_string());
    assert_eq!(
        [
            &sum["outcome"],
            &sum["color"],
            &sum["category"],
            &sum["rated"],
            &sum["baseMs"],
            &sum["incMs"],
            &sum["plies"]
        ],
        [
            &json!("win"),
            &json!("white"),
            &json!("3+2"),
            &json!(true),
            &json!(180_000),
            &json!(2000),
            &json!(5)
        ]
    );
    assert_eq!(sum["white"]["name"], "alice_api");
    assert_eq!(sum["black"]["name"], "bob_api");
    assert_eq!(call(b_api, "GET", "/account/games?result=loss", Some(&b_tok), None).await.1["total"], 1);
    assert_eq!(call(b_api, "GET", "/account/games?result=win", Some(&b_tok), None).await.1["total"], 0);
    assert_eq!(
        call(a_api, "GET", "/account/games?limit=0", Some(&a_tok), None).await.1["error"],
        "invalid_limit"
    );
    assert_eq!(call(a_api, "GET", "/account/games", None, None).await.0.status, 401);

    // The game record: caller-aware for its players, public otherwise.
    let (res, rec) = call(a_api, "GET", &format!("/games/{id}"), Some(&a_tok), None).await;
    assert_eq!(res.status, 200);
    assert_eq!((&rec["you"], &rec["reportable"]), (&json!("white"), &json!(true)));
    assert_eq!(call(b_api, "GET", &format!("/games/{id}"), Some(&b_tok), None).await.1["you"], "black");
    let (res, public) = call(a_api, "GET", &format!("/games/{id}"), None, None).await;
    assert_eq!(res.status, 200);
    assert!(public.get("you").is_none() && public.get("reportable").is_none(), "{public}");

    // The PGN.
    let (pgn, _) = call(a_api, "GET", &format!("/games/{id}/pgn"), Some(&a_tok), None).await;
    assert_eq!(pgn.status, 200);
    assert_eq!(pgn.header("content-type"), Some("application/x-chess-pgn; charset=utf-8"));
    let disposition = format!("attachment; filename=\"scacelith-{id}.pgn\"");
    assert_eq!(pgn.header("content-disposition"), Some(disposition.as_str()));
    let pgn = text(&pgn);
    for tag in [
        format!("[ScacelithGameId \"{id}\"]"),
        "[White \"alice_api\"]".into(),
        "[Black \"bob_api\"]".into(),
        "[Result \"1-0\"]".into(),
        "[TimeControl \"180+2\"]".into(),
        "[Termination \"normal\"]".into(),
        "[PlyCount \"5\"]".into(),
    ] {
        assert!(pgn.contains(&tag), "{tag} in {pgn}");
    }
    let first_move =
        pgn.lines().find(|l| l.starts_with("1. ")).unwrap_or_else(|| panic!("no movetext: {pgn}"));
    let clk =
        first_move.strip_prefix("1. e4 {[%clk ").unwrap_or_else(|| panic!("a clock comment: {first_move}"));
    let shape: String = clk.chars().take(9).map(|c| if c.is_ascii_digit() { 'd' } else { c }).collect();
    assert_eq!(shape, "d:dd:dd.d", "{first_move}");
    assert!(pgn.trim_end().ends_with("1-0"));

    // An e-mail change through its link.
    let seen_old = srv.mails_to(&alice.acc.email).len();
    let body = json!({"newEmail": "alice.new@example.org", "password": PASSWORD});
    let (res, answer) = call(a_api, "POST", "/account/email", Some(&a_tok), Some(body)).await;
    assert_eq!((res.status, answer), (202, json!({"status": "verification_sent"})));
    assert_eq!(a_api.me(&a_tok).await.expect("me")["user"]["pendingEmail"], "alice.new@example.org");
    let notice = srv.mail_to(&alice.acc.email, "was requested", 0).await;
    let notice_text = notice["text"].as_str().expect("text");
    assert!(
        notice_text.contains("a***@example.org") && !notice_text.contains("alice.new@example.org"),
        "{notice_text}"
    );
    let link_mail = srv.mail_to("alice.new@example.org", "Confirm your new e-mail address", 0).await;
    let lp = link_path(link_mail["text"].as_str().expect("text"));
    assert!(lp.starts_with("/confirm-email-change?token="), "{lp}");
    let form = page(&srv, "GET", &lp, None).await;
    assert_eq!(form.status, 200);
    assert!(text(&form).contains("alice.new@example.org"));
    let token = query_param(&lp, "token").expect("token");
    let ok = page(&srv, "POST", "/confirm-email-change", Some(&[("token", &token)])).await;
    assert_eq!(ok.status, 200);
    assert!(text(&ok).contains("E-mail address changed"), "{}", text(&ok));
    let me = a_api.me(&a_tok).await.expect("still signed in");
    assert_eq!(me["user"]["email"], "alice.new@example.org");
    assert_eq!(me["user"]["pendingEmail"], Value::Null);
    let changed = srv.mail_to(&alice.acc.email, "e-mail address was changed", 0).await;
    assert!(changed["text"].as_str().expect("text").contains("a***@example.org"));
    assert!(srv.mails_to(&alice.acc.email).len() >= seen_old + 2);

    // The export.
    let (res, doc) =
        call(a_api, "POST", "/account/export", Some(&a_tok), Some(json!({"password": PASSWORD}))).await;
    assert_eq!(res.status, 200, "{doc}");
    assert_eq!(
        res.header("content-disposition"),
        Some("attachment; filename=\"scacelith-account-alice_api.json\"")
    );
    assert_eq!(doc["format"], "scacelith-account-export");
    assert_eq!(doc["account"]["email"], "alice.new@example.org");
    assert_eq!(doc["games"]["total"], 1);
    assert_eq!(doc["games"]["list"][0]["id"].to_string().trim_matches('"'), id.to_string());
    let sessions = doc["sessions"].as_array().expect("sessions");
    assert!(sessions.iter().any(|s| s["clientLabel"] == "integration test"), "{sessions:?}");
    let ratings = doc["ratings"].as_array().expect("ratings");
    assert!(ratings.iter().any(|r| r["category"] == "3+2" && r["games"] == 1), "{ratings:?}");
    let (password_hash, token_hashes) = {
        let db = srv.db();
        let hash: String = db
            .query_row("SELECT password_hash FROM users WHERE id = ?1", [alice.acc.user_id], |r| r.get(0))
            .expect("the password hash");
        let mut q = db.prepare("SELECT token_hash FROM sessions WHERE user_id = ?1").expect("query");
        let tokens: Vec<String> = q
            .query_map([alice.acc.user_id], |r| r.get(0))
            .expect("rows")
            .map(|r| r.expect("a row"))
            .collect();
        (hash, tokens)
    };
    let exported = doc.to_string();
    assert!(!password_hash.is_empty() && !exported.contains(&password_hash), "no password hash");
    assert!(!token_hashes.is_empty());
    for h in &token_hashes {
        assert!(!exported.contains(h.as_str()), "no session token hash");
    }
    assert!(!exported.contains(&a_tok));

    // The deletion; it closes the live connection too.
    let (res, answer) =
        call(a_api, "POST", "/account/delete", Some(&a_tok), Some(json!({"password": PASSWORD}))).await;
    assert_eq!((res.status, answer), (200, json!({"status": "deleted"})));
    assert_eq!(closed_as_revoked(&mut alice).await, close::UNAUTHORIZED);
    let a_api = &alice.acc.api;
    assert_eq!(call(a_api, "GET", "/account/me", Some(&a_tok), None).await.0.status, 401);
    let export =
        call(a_api, "POST", "/account/export", Some(&a_tok), Some(json!({"password": PASSWORD}))).await;
    assert_eq!(export.0.status, 401);
    let login = json!({"login": "alice_api", "password": PASSWORD});
    assert_eq!(call(a_api, "POST", "/auth/login", None, Some(login)).await.0.status, 401);
    let deleted = format!("deleted#{}", alice.acc.user_id);
    assert_eq!(
        call(b_api, "GET", &format!("/games/{id}"), Some(&b_tok), None).await.1["white"]["name"],
        deleted
    );
    let hist = call(b_api, "GET", "/account/games", Some(&b_tok), None).await.1;
    assert_eq!(hist["games"][0]["white"]["name"], deleted);

    // A password reset revokes every session and closes the live connection.
    let forgot =
        call(b_api, "POST", "/auth/password/forgot", None, Some(json!({"email": bob.acc.email}))).await;
    assert_eq!(forgot.0.status, 202);
    let reset_mail = srv.mail_to(&bob.acc.email, "Reset your", 0).await;
    let reset = query_param(&link_path(reset_mail["text"].as_str().expect("text")), "token").expect("token");
    let body = json!({"token": reset, "newPassword": "another passphrase 42"});
    let (res, answer) = call(b_api, "POST", "/auth/password/reset", None, Some(body)).await;
    assert_eq!((res.status, answer), (200, json!({"status": "password_reset"})));
    assert_eq!(closed_as_revoked(&mut bob).await, close::UNAUTHORIZED);
    assert_eq!(call(&bob.acc.api, "GET", "/account/me", Some(&b_tok), None).await.0.status, 401);
}

/// Whether this process can run the server as `nobody` without the right to bind 443: root,
/// `setpriv`, and port 443 privileged.
fn can_drop_privileges() -> bool {
    let root = std::fs::metadata("/proc/self").is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0);
    let privileged = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|start| start > 443);
    let setpriv = std::process::Command::new("setpriv")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    root && privileged && setpriv
}

#[tokio::test]
async fn without_cap_net_bind_service_port_443_fails_with_the_fixes_in_the_log_and_exits_non_zero() {
    if !can_drop_privileges() {
        eprintln!(
            "skipped: needs root (to run the server as nobody), setpriv and ip_unprivileged_port_start > 443"
        );
        return;
    }
    let dir = TempDir::new("port");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let secret = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [9u8; 48]);
    let mut child = Command::new("setpriv")
        .args(["--reuid=65534", "--regid=65534", "--clear-groups", SERVER_BIN, "start"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("SERVER_SECRET", &secret)
        .env("DATA_DIR", dir.path())
        .env("WORKERS", "1")
        .env("TLS_MODE", "off")
        .env("ALLOW_INSECURE_DEV", "true")
        .env("BIND_ADDRESS", "127.0.0.1")
        .env("METRICS_PORT", "0")
        .env("LOG_FORMAT", "json")
        .env("LOG_LEVEL", "info")
        .env("MAIL_TRANSPORT", "none")
        .env("REQUIRE_EMAIL_VERIFICATION", "false")
        .env("SCACELITH_ENV_FILE", "")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("setpriv runs");
    let logs = Logs::default();
    logs.follow(child.stdout.take().expect("stdout"));
    logs.follow(child.stderr.take().expect("stderr"));
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap_or_else(|_| panic!("the server did not exit:\n{}", logs.tail(10)))
        .expect("the exit status");
    let hint = logs
        .wait(Duration::from_secs(5), |l| {
            l["msg"].as_str().is_some_and(|m| m.starts_with("Cannot listen on port 443 (EACCES)"))
        })
        .await
        .unwrap_or_else(|| panic!("the hint is logged:\n{}", logs.tail(10)));
    assert_eq!(hint["level"], "error");
    assert_eq!(hint["errorCode"], "EACCES");
    assert_eq!(hint["port"], 443);
    let msg = hint["msg"].as_str().expect("msg");
    for fix in [
        "AmbientCapabilities=CAP_NET_BIND_SERVICE",
        "setcap cap_net_bind_service=+ep",
        "net.ipv4.ip_unprivileged_port_start=443",
    ] {
        assert!(msg.contains(fix), "{fix} in {msg}");
    }
    assert_eq!(status.code(), Some(1), "{status}");
    assert!(logs.matching(|l| l["msg"] == "ready").is_empty(), "never ready");
}
