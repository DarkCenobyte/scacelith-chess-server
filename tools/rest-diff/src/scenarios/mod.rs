//! The scenarios, grouped in profiles: one profile is one pair of servers started with the same
//! settings, on which its scenarios run in order. Every scenario takes fresh loopback source
//! addresses and its own accounts, so that the per-address and per-account limits of one never
//! affect another: the servers keep their production limits, except where a profile says.

use std::future::Future;
use std::pin::Pin;

use crate::duo::Duo;

mod account;
mod auth;
mod basics;
mod games;
mod http_edge;
mod limits;
mod mfa;
mod pages;
mod pow;
mod variants;

/// The future of a scenario.
pub type BoxFut<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// A scenario: an async function over both servers.
pub type ScenarioFn = for<'a> fn(&'a mut Duo) -> BoxFut<'a>;

/// A pair of servers with its settings and its scenarios.
pub struct Profile {
    /// Name (`--profile`).
    pub name: &'static str,
    /// What it is for.
    pub about: &'static str,
    /// Settings on top of the common ones (`servers::base_env`).
    pub env: Vec<(&'static str, &'static str)>,
    /// Scenarios in order.
    pub scenarios: Vec<(&'static str, ScenarioFn)>,
}

/// The password of the scenarios' accounts.
pub const PASSWORD: &str = "correct horse battery";

/// Registers `user` with `email`, follows the confirmation link of its mail (the page's POST,
/// as a browser would) and signs in; saves the session token as `<user>.token`. Every request
/// is a compared step.
pub async fn new_account(d: &mut Duo, ip: std::net::IpAddr, user: &str, email: &str) {
    use crate::http::Req;
    d.step(&format!("{user}-register"), ip, 202, |_| {
        Req::post("/api/v1/auth/register")
            .json(serde_json::json!({"username": user, "email": email, "password": PASSWORD}))
    })
    .await;
    let link = format!("{user}.verify");
    d.mail(&format!("{user}-verification-mail"), email, Some(&link)).await;
    d.step(&format!("{user}-verify"), ip, 200, |s| {
        Req::post("/verify-email").form(&[("token", &s.v(&link))])
    })
    .await;
    login(d, ip, user, PASSWORD, &format!("{user}.token")).await;
}

/// Signs in `login` with `password` and saves the token as `var`.
pub async fn login(
    d: &mut Duo,
    ip: std::net::IpAddr,
    login: &str,
    password: &str,
    var: &str,
) -> crate::duo::Pair {
    use crate::http::Req;
    let p = d
        .step(&format!("{var}-login"), ip, 200, |_| {
            Req::post("/api/v1/auth/login").json(serde_json::json!({"login": login, "password": password}))
        })
        .await;
    d.save(&p, var, "token");
    p
}

/// Every profile, in the order they run.
pub fn profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "main",
            about: "production limits and e-mail verification; no proof of work at registration",
            env: vec![
                ("POW_REGISTER_BITS", "0"),
                // Failed sign-ins of the scenarios must not start a proof-of-work wave (the
                // `pow` profile tests it).
                ("POW_LOGIN_TRIGGER_PER_MIN", "100000"),
                // Two counted games put a player on the leaderboard.
                ("PROVISIONAL_GAMES", "2"),
                ("MATCH_REPEAT_LIMIT", "1000"),
                ("CHALLENGE_UNPLAYED_PER_MIN", "1000"),
            ],
            scenarios: vec![
                ("basics", basics::run),
                ("http-edge", http_edge::run),
                ("signup", auth::signup),
                ("login", auth::sign_in),
                ("sessions", auth::sessions),
                ("password", auth::password),
                ("email-change", account::email_change),
                ("account", account::account),
                ("mfa", mfa::run),
                ("games", games::run),
                ("pages", pages::run),
                ("limits", limits::run),
            ],
        },
        Profile {
            name: "pow",
            about: "proof of work at registration and after a wave of failed sign-ins",
            env: vec![
                ("POW_REGISTER_BITS", "10"),
                ("POW_LOGIN_BITS", "8"),
                ("POW_LOGIN_TRIGGER_PER_MIN", "3"),
            ],
            scenarios: vec![("pow", pow::run)],
        },
        Profile {
            name: "open",
            about: "no e-mail verification (accounts ready at once, immediate e-mail changes)",
            env: vec![("REQUIRE_EMAIL_VERIFICATION", "false"), ("POW_REGISTER_BITS", "0")],
            scenarios: vec![("open", variants::open)],
        },
        Profile {
            name: "closed",
            about: "registration closed, a message of the day",
            env: vec![
                ("REGISTRATION", "closed"),
                ("SERVER_MOTD", "Maintenance at 18:00 UTC"),
                ("POW_REGISTER_BITS", "0"),
            ],
            scenarios: vec![("closed", variants::closed)],
        },
        Profile {
            name: "custom",
            about: "GIFs off, no custom time controls, two categories, other account rules, a 2 KiB body limit",
            env: vec![
                ("GIF_ENABLED", "false"),
                ("ALLOW_CUSTOM_TIME_CONTROLS", "false"),
                ("RATED_CATEGORIES", "3+2,10+0"),
                ("USERNAME_MIN", "4"),
                ("USERNAME_MAX", "16"),
                ("PASSWORD_MIN_LENGTH", "12"),
                ("HTTP_BODY_LIMIT", "2048"),
                ("POW_REGISTER_BITS", "0"),
            ],
            scenarios: vec![("custom", variants::custom)],
        },
        Profile {
            name: "proxy",
            about: "TLS_MODE=proxy: plain HTTP behind a trusted proxy, X-Forwarded-For",
            env: vec![("TLS_MODE", "proxy"), ("TRUSTED_PROXIES", "127.0.0.1"), ("POW_REGISTER_BITS", "0")],
            scenarios: vec![("proxy", variants::proxy)],
        },
        Profile {
            name: "abuse",
            about: "the per-address layer with low numbers: its threshold, the address block, requests in progress",
            env: vec![
                ("HTTP_RATE_PER_IP", "60"),
                ("ABUSE_BLOCK_REFUSALS_PER_MIN", "10"),
                ("ABUSE_BLOCK_BASE_SEC", "3"),
                ("IP_MAX_INFLIGHT", "2"),
                ("POW_REGISTER_BITS", "0"),
            ],
            scenarios: vec![("abuse", variants::abuse)],
        },
    ]
}
