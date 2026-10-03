//! `GET /info`, ported from the Node suite `auth.account` (the document) with the server id
//! cache of `info.js`.

use serde_json::json;

use super::super::read_support::*;
use super::*;

const NOW: i64 = 1_790_856_000_000;

fn club_config() -> Config {
    config(&[
        ("SERVER_NAME", "Club"),
        ("SERVER_MOTD", "Welcome"),
        ("WS_PORT", "9444"),
        ("RATED_CATEGORIES", "3+2,10+0"),
        ("POW_REGISTER_BITS", "16"),
        ("REQUIRE_EMAIL_VERIFICATION", "1"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
        ("API_PORT", "8443"),
    ])
}

async fn start(config: Config) -> Server {
    let store = memory_store(&config).await;
    let s = store.clone();
    Server::start(config, store, NOW, move |router, config| {
        register(router, InfoDeps { config: config.clone(), store: s });
    })
}

#[tokio::test]
async fn the_info_document() {
    let s = start(club_config()).await;
    s.store.meta().set("server_id".into(), "test-server-id".into()).await.unwrap();
    let r = s.t.get("/api/v1/info").send().await;
    assert_eq!(r.status, 200);
    let expected = json!({
        "name": "Club", "serverId": "test-server-id", "motd": "Welcome",
        "protocol": { "min": 1, "max": PROTOCOL_VERSION, "schema": FINGERPRINT, "subprotocol": "scacelith.rt1" },
        "wsPort": 9444, "wsPath": "/ws", "registration": "open", "emailVerification": true, "sso": { "google": false },
        "mfa": true, "pow": { "register": 16 },
        "categories": [{ "id": "3+2", "baseSec": 180, "incSec": 2 }, { "id": "10+0", "baseSec": 600, "incSec": 0 }],
        "limits": {
            "usernameMin": 3, "usernameMax": 20, "usernamePattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$",
            "passwordMinLength": 10, "passwordMaxBytes": 256, "customTimeControls": true, "reportsPerDay": 5,
            "wsMaxMessageBytes": 512,
        },
    });
    assert_eq!(r.json(), expected);
    // Key order, as the Node server wrote it.
    let body = r.text();
    let order = [
        "\"name\"",
        "\"serverId\"",
        "\"motd\"",
        "\"protocol\"",
        "\"wsPort\"",
        "\"wsPath\"",
        "\"registration\"",
        "\"emailVerification\"",
        "\"sso\"",
        "\"mfa\"",
        "\"pow\"",
        "\"categories\"",
        "\"limits\"",
    ];
    let at: Vec<usize> = order.iter().map(|k| body.find(k).unwrap()).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{body}");
    assert!(body.contains("\"schema\":97842216,"), "the fingerprint as a decimal integer: {body}");
    // No session, no limit of its own.
    assert_eq!(s.t.get("/api/v1/info").bearer("sct_nobody").send().await.status, 200);
}

#[tokio::test]
async fn the_server_id_is_read_until_the_store_has_one_then_kept() {
    let s = start(club_config()).await;
    let real = s.store.server_id().await.unwrap().expect("the migration creates the server id");
    assert_eq!(s.t.get("/api/v1/info").send().await.json()["serverId"], real.as_str());
    s.store.meta().set("server_id".into(), "changed".into()).await.unwrap();
    assert_eq!(s.t.get("/api/v1/info").send().await.json()["serverId"], real.as_str(), "read once");

    // A store that cannot answer: null, and read again on the next request.
    let s = start(club_config()).await;
    s.store.close().await;
    assert_eq!(s.t.get("/api/v1/info").send().await.json()["serverId"], serde_json::Value::Null);
}

#[test]
fn categories_in_seconds_sso_and_registration() {
    let c = config(&[
        ("RATED_CATEGORIES", "1+0,15+10"),
        ("REGISTRATION", "closed"),
        ("SSO_GOOGLE_ENABLED", "1"),
        ("GOOGLE_CLIENT_ID", "id.apps.googleusercontent.com"),
        ("GOOGLE_CLIENT_SECRET", "secret"),
        ("REQUIRE_EMAIL_VERIFICATION", "1"),
        ("SERVER_PUBLIC_HOST", "chess.example.org"),
    ]);
    let doc = info_document(&c);
    assert_eq!(
        doc["categories"],
        json!([{ "id": "1+0", "baseSec": 60, "incSec": 0 }, { "id": "15+10", "baseSec": 900, "incSec": 10 }])
    );
    assert_eq!(
        (doc["registration"].clone(), doc["sso"].clone()),
        (json!("closed"), json!({ "google": true }))
    );
    assert_eq!(seconds(90_500), json!(90.5));
    assert_eq!(seconds(180_000), json!(180));
}
