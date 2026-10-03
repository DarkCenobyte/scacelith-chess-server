//! The sso part: Google sign-in (docs/API.md "Google sign-in"). The whole server runs in this
//! process ([`scacelith_server::embedded`]) because Google's token and key endpoints must be a
//! local fake provider ([`FakeGoogle`]), which no setting may name. Its public host is LIVE_HOST
//! and its API port a fixed one, so that its origin tag is the one the game computes for the
//! server it connects to; e-mail confirmation is on. TLS with a pinned certificate by default,
//! plain HTTP on the loopback with `--sso-http` (the C++ client is then a development one).
//!
//! The C++ test `net_live_sso` signs in through its 127.0.0.1 listener; its browser opener asks
//! the control server for Google's answer (`GET /fake-authorize`). The control server also seeds
//! accounts (`POST /seed-password-account`), gives TOTP codes (`GET /totp`) and lists the Google
//! links stored (`GET /links`).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use scacelith_client::{ApiClient, Endpoint, Login, TlsConfig};
use scacelith_server::auth::form_urlencode;
use scacelith_server::config::{LoadOptions, load};
use scacelith_server::embedded;
use scacelith_server::log::{Level, capture_logs};
use scacelith_server::security::password::{Argon2Hasher, PasswordHasher};
use scacelith_server::store::{clean_email, normalize_email};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::control::{Call, Control, Reply, Routes};
use crate::google::{Consent, FakeGoogle};
use crate::support::TempDir;
use crate::support::server::{base_env, certificate, certificate_pin, free_port};
use crate::support::web::call;
use crate::{Ctx, fresh_totp, run_cpp};

/// The OAuth client of the server at the fake Google.
const CLIENT_ID: &str = "live-check.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "GOCSPX-live-check";
/// What Google adds to its redirect besides code, state and iss (the game forwards only those
/// three).
const GOOGLE_EXTRA: &[(&str, &str)] = &[
    (
        "scope",
        "email profile openid https://www.googleapis.com/auth/userinfo.email \
         https://www.googleapis.com/auth/userinfo.profile",
    ),
    ("authuser", "0"),
    ("prompt", "consent"),
];
/// The client label of the harness's own sessions.
const HARNESS_LABEL: &str = "live harness";

pub(crate) async fn run(ctx: &Ctx) -> i32 {
    let dir = TempDir::new("live-sso");
    let google = match FakeGoogle::start(CLIENT_ID, CLIENT_SECRET).await {
        Ok(google) => google,
        Err(e) => {
            eprintln!("[sso] the fake Google: {e}");
            return 1;
        }
    };
    let port = free_port();
    let mut env = base_env(&dir, 1, 0);
    let mut set = |key: &str, value: &str| {
        env.retain(|(k, _)| k != key);
        env.push((key.to_owned(), value.to_owned()));
    };
    set("SERVER_PUBLIC_HOST", &ctx.host);
    set("API_PORT", &port.to_string());
    set("REQUIRE_EMAIL_VERIFICATION", "true");
    set("SSO_GOOGLE_ENABLED", "true");
    set("GOOGLE_CLIENT_ID", CLIENT_ID);
    set("GOOGLE_CLIENT_SECRET", CLIENT_SECRET);
    if ctx.sso_http {
        set("TLS_MODE", "off");
        set("ALLOW_INSECURE_DEV", "1");
    }
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let config = match load(&LoadOptions::from_pairs(&pairs)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[sso] configuration: {e}");
            return 1;
        }
    };
    let tag = config.describe()["ssoRedirectTag"].as_str().unwrap_or_default().to_owned();

    // The server logs in this process: into the capture, printed at the end (a failed Google
    // sign-in gives its reason only there, as a warning).
    let capture = capture_logs(Level::Info);
    let server = match embedded::start(config, google.options()).await {
        Ok(server) => server,
        Err(e) => {
            drop(capture);
            eprintln!("[sso] the server did not start: {e}");
            return 1;
        }
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let (_, _, pem) = certificate(&dir);
    let (endpoint, pin, mode) = if ctx.sso_http {
        (Endpoint::plain(addr), None, "plain HTTP".to_owned())
    } else {
        let tls = TlsConfig::with_root_pem(&pem).expect("the certificate of the server");
        let pin = certificate_pin(&pem);
        let mode = format!("TLS, certificate SHA-256 {pin}");
        (Endpoint::tls(addr, "localhost", tls), Some(pin), mode)
    };
    println!("[sso] server on {}:{port} ({mode}), origin tag {tag}", ctx.host);

    let code = match Control::bind().await {
        Ok(ctl) => {
            let routes = SsoRoutes {
                dir: &dir,
                google: &google,
                api: ApiClient::new(endpoint),
                state: json!({"host": ctx.host, "apiPort": port, "tag": tag, "clientId": CLIENT_ID}),
                totp_steps: Mutex::new(HashMap::new()),
                secrets: Mutex::new(HashMap::new()),
            };
            let live = format!("{}:{port}:{}", ctx.host, ctl.port);
            let mut vars = vec![("SCACELITH_NET_LIVE_SSO", live.as_str())];
            if let Some(pin) = &pin {
                vars.push(("SCACELITH_NET_LIVE_SSO_PIN", pin));
            }
            tokio::select! {
                code = run_cpp(ctx, "net_live_sso", &vars) => code,
                () = ctl.serve(&routes) => 1,
            }
        }
        Err(e) => {
            eprintln!("[sso] control server: {e}");
            1
        }
    };
    println!("[sso] C++ test exit code {code}");
    server.stop().await;

    let records = capture.records();
    drop(capture);
    let mails: Vec<String> = records
        .iter()
        .filter(|r| r["msg"] == "mail (log transport)")
        .map(|r| format!("{} \"{}\"", r["to"].as_str().unwrap_or(""), r["subject"].as_str().unwrap_or("")))
        .collect();
    println!("[sso] mails: {}", if mails.is_empty() { "none".to_owned() } else { mails.join(", ") });
    let logged: Vec<&Value> =
        records.iter().filter(|r| r["level"] == "warn" || r["level"] == "error").collect();
    if !logged.is_empty() {
        println!("[sso] server warnings and errors logged:");
        for r in logged {
            println!("{r}");
        }
    }
    code
}

/// The control routes of `net_live_sso`.
struct SsoRoutes<'a> {
    dir: &'a TempDir,
    google: &'a FakeGoogle,
    api: ApiClient,
    state: Value,
    /// The last TOTP step handed out per secret.
    totp_steps: Mutex<HashMap<String, i64>>,
    /// The authenticator secret of each account seeded with two-step verification, by lower-cased
    /// username.
    secrets: Mutex<HashMap<String, String>>,
}

impl SsoRoutes<'_> {
    /// A connection to the server's database (the server keeps running: WAL, busy wait).
    fn db(&self) -> rusqlite::Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.dir.file("scacelith.db"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(db)
    }

    /// `{username, email, password?, mfa?}`: an account whose address is confirmed, as a used
    /// registration link leaves it; without a password, one like a Google-made account. With
    /// `mfa`: two-step verification turned on through the API; the answer gives its `totpSecret`.
    async fn seed(&self, body: &Value) -> Reply {
        let (Some(username), Some(email)) = (body["username"].as_str(), body["email"].as_str()) else {
            return Reply::error(400, "username_and_email");
        };
        let password = body["password"].as_str().filter(|p| !p.is_empty()).map(str::to_owned);
        let mfa = body["mfa"].as_bool().unwrap_or(false);
        if mfa && password.is_none() {
            return Reply::error(400, "mfa_needs_password");
        }
        let hash = match password.clone() {
            Some(pw) => match tokio::task::spawn_blocking(move || Argon2Hasher::default().hash(&pw)).await {
                Ok(Ok(hash)) => Some(hash),
                failure => return harness_error(format!("password hash: {failure:?}")),
            },
            None => None,
        };
        let inserted = self.db().and_then(|db| {
            db.execute(
                "INSERT INTO users (username, username_lower, email, email_normalized, email_verified, password_hash,
                 accept_challenges, created_at) VALUES (?1, ?2, ?3, ?4, 1, ?5, 1, ?6)",
                rusqlite::params![
                    username,
                    username.to_lowercase(),
                    clean_email(email),
                    normalize_email(email),
                    hash,
                    crate::now_ms()
                ],
            )?;
            Ok(db.last_insert_rowid())
        });
        let user_id = match inserted {
            Ok(id) => id,
            Err(e) => return harness_error(format!("insert {username}: {e}")),
        };
        let Some(password) = password.filter(|_| mfa) else { return Reply::ok(json!({ "userId": user_id })) };

        let token = match self.api.login(username, &password, Some(HARNESS_LABEL)).await {
            Ok(Login::Session(session)) => session.token,
            other => {
                return Reply::with_status(500, json!({"error": "login", "detail": format!("{other:?}")}));
            }
        };
        let (res, setup) = call(
            &self.api,
            "POST",
            "/account/mfa/totp/setup",
            Some(&token),
            Some(json!({ "password": password })),
        )
        .await;
        let Some(secret) = setup["secret"].as_str().map(str::to_owned).filter(|_| res.status == 200) else {
            return Reply::with_status(
                500,
                json!({"error": "mfa_setup", "status": res.status, "body": setup}),
            );
        };
        let code = fresh_totp(&mut *self.totp_steps.lock().await, &secret)["code"].clone();
        let (res, on) =
            call(&self.api, "POST", "/account/mfa/totp/enable", Some(&token), Some(json!({ "code": code })))
                .await;
        if res.status != 200 {
            return Reply::with_status(500, json!({"error": "mfa_enable", "status": res.status, "body": on}));
        }
        let _ = call(&self.api, "POST", "/auth/logout", Some(&token), Some(json!({}))).await;
        self.secrets.lock().await.insert(username.to_lowercase(), secret.clone());
        Reply::ok(json!({ "userId": user_id, "totpSecret": secret }))
    }

    /// `?url=<authUrl of a start answer>`, then the Google account picked (`?sub=&email=&name=`,
    /// `?verified=false` for an address Google has not confirmed) or `?error=access_denied`:
    /// Google's 302 to the redirect URI (`Location`, also given as `{location}`), with the
    /// parameters Google adds.
    fn fake_authorize(&self, c: &Call) -> Reply {
        let or = |v: &str, default: &str| if v.is_empty() { default.to_owned() } else { v.to_owned() };
        let error = c.q("error");
        let consent = if error.is_empty() {
            Consent::Account(json!({
                "sub": or(c.q("sub"), "1000001"),
                "email": or(c.q("email"), "live.player@gmail.com"),
                "email_verified": c.q("verified") != "false",
                "name": or(c.q("name"), "Live Player"),
            }))
        } else {
            Consent::Refused(error.to_owned())
        };
        match self.google.authorize(c.q("url"), consent) {
            Ok((redirect_uri, query)) => {
                let mut location = format!("{redirect_uri}?{query}");
                if error.is_empty() {
                    location.push('&');
                    location.push_str(&form_urlencode(GOOGLE_EXTRA));
                }
                Reply {
                    status: 302,
                    body: json!({ "location": location }),
                    headers: vec![("location".to_owned(), location)],
                }
            }
            Err(e) => Reply::with_status(400, json!({"error": "bad_authorization_request", "message": e})),
        }
    }

    /// The Google links stored (`sso_identities`), oldest first.
    fn links(&self) -> rusqlite::Result<Value> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT s.provider, s.subject, s.user_id, u.username, s.email, s.created_at
             FROM sso_identities s JOIN users u ON u.id = s.user_id ORDER BY s.created_at, s.subject",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "provider": r.get::<_, String>(0)?,
                "subject": r.get::<_, String>(1)?,
                "userId": r.get::<_, i64>(2)?,
                "username": r.get::<_, String>(3)?,
                "email": r.get::<_, Option<String>>(4)?,
                "createdAt": r.get::<_, i64>(5)?,
            }))
        })?;
        Ok(json!({ "links": rows.collect::<rusqlite::Result<Vec<Value>>>()? }))
    }
}

impl Routes for SsoRoutes<'_> {
    async fn answer(&self, c: &Call) -> Reply {
        match (c.method.as_str(), c.path.as_str()) {
            ("GET", "/state") => Reply::ok(self.state.clone()),
            ("POST", "/seed-password-account") => self.seed(&c.body).await,
            // ?username= (an account seeded with mfa) or ?secret=<base32>
            ("GET", "/totp") => {
                let secret = match c.q("secret") {
                    "" => self.secrets.lock().await.get(&c.q("username").to_lowercase()).cloned(),
                    s => Some(s.to_owned()),
                };
                match secret {
                    Some(secret) => Reply::ok(fresh_totp(&mut *self.totp_steps.lock().await, &secret)),
                    None => Reply::error(404, "no_secret"),
                }
            }
            ("GET", "/fake-authorize") => self.fake_authorize(c),
            ("GET", "/links") => match self.links() {
                Ok(links) => Reply::ok(links),
                Err(e) => harness_error(e.to_string()),
            },
            _ => Reply::error(404, "no_route"),
        }
    }
}

/// A failure of the harness itself: 500 `{error: "harness", message}`.
fn harness_error(message: String) -> Reply {
    Reply::with_status(500, json!({ "error": "harness", "message": message }))
}
