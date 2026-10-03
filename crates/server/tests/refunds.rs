//! Rating refunds end to end (docs/ANTICHEAT.md, rating refunds), on the real server with two
//! shards: a player who beat three rated opponents is banned automatically for an illegal move;
//! each opponent gets the points they lost back, and `Notice{RatingRestored}` out of a game only:
//! at once for the one who is connected and idle, after the game in progress for the one who is
//! playing, right after Welcome for the one who was offline. Then a player confirmed as a cheater
//! with the admin CLI (another process) while playing: the game in progress is refunded when it is
//! recorded, and the player starts no other game.
//!
//! Port of the Node.js `test/integration/refunds.test.js`.

#[macro_use]
mod support;

use std::time::Duration;

use scacelith_protocol::{ColorPref, EndReason, GameStatus, NoticeCode, ServerMsg, close};
use serde_json::Value;
use support::*;

/// A server like the Node.js suite's: two shards, a long first-move deadline, fast commits.
async fn server() -> TestServer {
    TestServer::options()
        .workers(2)
        .env("FIRST_MOVE_TIMEOUT_MS", "20000")
        .env("DB_COMMIT_MS", "20")
        .start()
        .await
}

/// Established players (40 games, K 20) in 3+2, so that a lost game costs K-formula points.
fn seed_rated(srv: &TestServer, players: &[&Player]) {
    let db = srv.db();
    for p in players {
        db.execute(
            "INSERT INTO ratings (user_id, category, rating, games, wins, losses, peak, rated, counted_games, updated_at)
             VALUES (?1, '3+2', 1500, 40, 20, 20, 1500, 1, 40, 0)",
            [p.acc.user_id],
        )
        .expect("a seeded rating");
    }
}

/// The 3+2 rating of a player.
fn rating_of(srv: &TestServer, user_id: u32) -> i64 {
    srv.db()
        .query_row("SELECT rating FROM ratings WHERE user_id = ?1 AND category = '3+2'", [user_id], |r| {
            r.get(0)
        })
        .expect("a rating")
}

/// Waits until the 3+2 rating of a player is `rating` (a refund is written by the server's
/// database writer, a moment after the decision).
async fn rating_becomes(srv: &TestServer, user_id: u32, rating: i64) {
    let what = format!("the rating {rating} of user {user_id}");
    eventually(Duration::from_secs(5), &what, || async { (rating_of(srv, user_id) == rating).then_some(()) })
        .await;
}

/// A rated 3+2 game that `winner` (White) wins by resignation after two moves; the points the
/// loser lost once the game is rated.
async fn lose_to(winner: &mut Player, loser: &mut Player) -> i64 {
    let id = challenge_game(winner, loser, 180, 2, true).await;
    let mut t = Table::new(id);
    t.play_all(winner, loser, &["e2e4", "e7e5"]).await;
    let m = loser.client.mark();
    loser.client.resign(id);
    let ru = wait_msg!(loser.client, m, ServerMsg::RatingUpdate(r) if r.game == id => r.clone());
    i64::from(ru.black.before) - i64::from(ru.black.after)
}

/// The points of the `RatingRestored` notice `p` received since `since`.
async fn restored(p: &mut Player, since: usize, limit: Duration) -> i64 {
    wait_msg!(p.client, since, limit, ServerMsg::Notice(n) if n.code == NoticeCode::RatingRestored => n.arg as i64)
}

/// Whether a `RatingRestored` notice reaches `p` within `limit` (a bounded observation: what must
/// not happen).
async fn restored_within(p: &mut Player, since: usize, limit: Duration) -> bool {
    p.client
        .try_wait_for(since, limit, |m| {
            matches!(m, ServerMsg::Notice(n) if n.code == NoticeCode::RatingRestored).then_some(())
        })
        .await
        .is_some()
}

#[tokio::test]
async fn a_banned_cheaters_victims_get_their_points_back_and_are_told_out_of_a_game_only() {
    let srv = server().await;
    let [mut cheat, mut idle, mut busy, mut away, mut other, mut target] =
        players(&srv, ["cheat", "idle", "busy", "away", "other", "target"]).await;
    seed_rated(&srv, &[&cheat, &idle, &busy, &away]);
    let lost_idle = lose_to(&mut cheat, &mut idle).await;
    let lost_busy = lose_to(&mut cheat, &mut busy).await;
    let lost_away = lose_to(&mut cheat, &mut away).await;
    for lost in [lost_idle, lost_busy, lost_away] {
        assert!((9..=10).contains(&lost), "K 20, the cheater 0 to 20 points higher: {lost}");
    }
    let cheat_rating = rating_of(&srv, cheat.acc.user_id);

    // `away` goes offline; `busy` starts a game with `other`.
    away.client.close().await;
    let g = challenge_game(&mut busy, &mut other, 180, 2, false).await;
    let mut t = Table::new(g);
    t.play(&mut busy, &mut other, "d2d4").await;
    let (m_idle, m_busy) = (idle.client.mark(), busy.client.mark());

    // The cheat: an illegal move in a synchronised position bans the cheater at once.
    let cg = challenge_game(&mut cheat, &mut target, 180, 2, false).await;
    let mut ct = Table::new(cg);
    ct.play_all(&mut cheat, &mut target, &["e2e4", "e7e5"]).await;
    ct.send(&cheat, "e1e3");
    assert_eq!(cheat.client.closed(Duration::from_secs(5)).await.code, close::CHEAT_DETECTED);

    // Connected and idle: told at once, with the points given back.
    assert_eq!(restored(&mut idle, m_idle, Duration::from_secs(5)).await, lost_idle);
    rating_becomes(&srv, idle.acc.user_id, 1500).await;
    // Playing: the refund is applied, the notice waits for the end of the game.
    rating_becomes(&srv, busy.acc.user_id, 1500).await;
    assert!(!restored_within(&mut busy, m_busy, Duration::from_secs(1)).await, "no notice during a game");
    let m_end = busy.client.mark();
    busy.client.resign(g);
    let end = wait_msg!(busy.client, m_end, ServerMsg::GameEnd(e) if e.game == g => e.clone());
    assert_eq!((end.status, end.reason), (GameStatus::BlackWins, EndReason::Resignation));
    assert_eq!(restored(&mut busy, m_end, Duration::from_secs(5)).await, lost_busy);
    // Offline: told right after Welcome at the next connection.
    rating_becomes(&srv, away.acc.user_id, 1500).await;
    away.reconnect(&srv).await;
    assert_eq!(restored(&mut away, 0, Duration::from_secs(5)).await, lost_away);

    // Each refund is marked notified (once the shard has answered that it wrote the notice,
    // which may come a moment after the client read it); the cheater's own rating is left alone.
    let refunds = || -> Vec<(u32, i64, String, Option<i64>)> {
        let db = srv.db();
        let mut q = db
            .prepare("SELECT victim_id, points, source, notified_at FROM rating_refunds ORDER BY victim_id")
            .expect("query");
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("rows")
            .map(|r| r.expect("a row"))
            .collect()
    };
    let rows = eventually(Duration::from_secs(5), "the refunds marked notified", || {
        let rows = refunds();
        async move { rows.iter().all(|r| r.3.is_some_and(|at| at > 0)).then_some(rows) }
    })
    .await;
    let mut expected = vec![
        (idle.acc.user_id, lost_idle, "auto".to_string()),
        (busy.acc.user_id, lost_busy, "auto".to_string()),
        (away.acc.user_id, lost_away, "auto".to_string()),
    ];
    expected.sort();
    assert_eq!(rows.iter().map(|r| (r.0, r.1, r.2.clone())).collect::<Vec<_>>(), expected);
    assert_eq!(rating_of(&srv, cheat.acc.user_id), cheat_rating);
}

#[tokio::test]
async fn a_cheater_confirmed_with_the_cli_while_playing_is_refunded_when_recorded_and_plays_no_more() {
    let srv = server().await;
    let [mut cheat, mut vic, mut next] = players(&srv, ["clicheat", "clivic", "clinext"]).await;
    seed_rated(&srv, &[&cheat, &vic, &next]);

    // A rated game is in progress when the moderator confirms (the CLI cannot reach the server).
    let g = challenge_game(&mut cheat, &mut vic, 180, 2, true).await;
    let mut t = Table::new(g);
    t.play_all(&mut cheat, &mut vic, &["e2e4", "e7e5"]).await;
    let out = srv
        .admin(
            &["integrity", "confirm", "clicheat", "--reason", "engine", "--json"],
            &[("SCACELITH_MODERATOR", "mod")],
        )
        .await
        .unwrap_or_else(|e| panic!("the admin command: {e}"));
    let conf: Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("JSON from the admin command ({e}): {out}"));
    assert_eq!(conf["refunds"], Value::Array(Vec::new()), "no game against the cheater recorded yet: {conf}");

    // The victim resigns: the game keeps its rating change, the points come back as it is
    // recorded, and the victim is told once out of the game.
    let m = vic.client.mark();
    vic.client.resign(g);
    let ru = wait_msg!(vic.client, m, ServerMsg::RatingUpdate(r) if r.game == g => r.clone());
    let lost = i64::from(ru.black.before) - i64::from(ru.black.after);
    assert!((9..=10).contains(&lost), "K 20, equal ratings: {lost}");
    assert_eq!(restored(&mut vic, m, Duration::from_secs(10)).await, lost);
    rating_becomes(&srv, vic.acc.user_id, 1500).await;
    let rows: Vec<(i64, u32, i64, String, Option<i64>)> = {
        let db = srv.db();
        let mut q = db
            .prepare("SELECT game_id, victim_id, points, source, sanction_id FROM rating_refunds WHERE cheater_id = ?1")
            .expect("query");
        q.query_map([cheat.acc.user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .expect("rows")
            .map(|r| r.expect("a row"))
            .collect()
    };
    let sanction = conf["sanctionId"].as_i64();
    assert!(sanction.is_some(), "the sanction of the confirmation: {conf}");
    assert_eq!(rows, [(g as i64, vic.acc.user_id, lost, "auto".to_string(), sanction)]);

    // The cheater, still connected, challenges someone else: refused, disconnected as banned.
    let m_next = next.client.mark();
    cheat.client.challenge(next.name(), 180, 2, true, ColorPref::White);
    assert_eq!(cheat.client.closed(Duration::from_secs(5)).await.code, close::BANNED);
    let reached = next
        .client
        .try_wait_for(m_next, Duration::from_millis(300), |m| {
            matches!(m, ServerMsg::ChallengeReceived(_)).then_some(())
        })
        .await;
    assert!(reached.is_none(), "no challenge reached anyone");
}
