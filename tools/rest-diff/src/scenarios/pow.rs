//! Proof of work (docs/API.md section 1.6): the registration's challenge and each reason of a
//! refusal (`required`, `malformed`, `signature`, `endpoint`, `network`, `expired`, `work`,
//! `replayed`), the order of the checks, and the wave of failed sign-ins that makes the sign-in
//! ask for a proof (profile `pow`: 10 bits at registration, 8 at sign-in, a wave from 3 failures
//! in a minute).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{BoxFut, PASSWORD};
use crate::crypto;
use crate::duo::{Duo, Side, fresh_ip};
use crate::http::Req;

/// The proof for the challenge saved as `var` (its bits saved as `<var>.bits`).
fn proof(s: &Side, var: &str) -> Value {
    let challenge = s.v(var);
    let bits = s.v(&format!("{var}.bits")).parse().unwrap_or(0);
    json!({"challenge": challenge, "nonce": crypto::solve_pow(&challenge, bits)})
}

/// The challenge saved as `var` with the first character of its signature changed.
fn tampered(s: &Side, var: &str) -> String {
    let c = s.v(var);
    match c.rfind('.') {
        Some(dot) if dot + 1 < c.len() => {
            let first = &c[dot + 1..dot + 2];
            let other = if first == "A" { "B" } else { "A" };
            format!("{}{other}{}", &c[..=dot], &c[dot + 2..])
        }
        _ => format!("{c}x"),
    }
}

fn register(user: &str, pow: Option<Value>) -> Req {
    let mut body = json!({"username": user, "email": format!("{user}@example.org"), "password": PASSWORD});
    if let Some(p) = pow {
        body["pow"] = p;
    }
    Req::post("/api/v1/auth/register").json(body)
}

/// Saves the challenge of a 428 answer as `var` and its bits as `<var>.bits`.
fn save_challenge(d: &mut Duo, pair: &crate::duo::Pair, var: &str) {
    d.save(pair, var, "pow.challenge");
    d.save(pair, &format!("{var}.bits"), "pow.bits");
}

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        let ip0 = fresh_ip();
        d.step("info", ip, 200, |_| Req::get("/api/v1/info")).await;
        // A challenge kept for the `expired` refusal at the end.
        let p = d.step("challenge-to-expire", ip0, 428, |_| register("ana", None)).await;
        save_challenge(d, &p, "cx");
        let issued = Instant::now();

        // Checks before the proof (from another address: 10 registrations per hour per client).
        let ipb = fresh_ip();
        d.step("register-invalid-username", ipb, 400, |_| register("a", None)).await;
        d.step("register-weak-password", ipb, 400, |_| {
            Req::post("/api/v1/auth/register")
                .json(json!({"username": "bea", "email": "bea@example.org", "password": "short"}))
        })
        .await;
        let p = d.step("register-no-pow", ip, 428, |_| register("bea", None)).await;
        save_challenge(d, &p, "c1");
        d.step("register-pow-string", ipb, 0, |_| register("bea", Some(json!("abc")))).await;
        d.step("register-pow-empty", ipb, 0, |_| register("bea", Some(json!({})))).await;
        d.step("register-pow-extra-field", ip, 0, |s| {
            let mut p = proof(s, "c1");
            p["x"] = json!(1);
            register("bea", Some(p))
        })
        .await;
        d.step("register-challenge-short", ipb, 400, |_| {
            register("bea", Some(json!({"challenge": "abc", "nonce": "1"})))
        })
        .await;
        d.step("register-malformed", ipb, 428, |_| {
            register("bea", Some(json!({"challenge": "abcdefghijklmnopqrstuvwxyz", "nonce": "1"})))
        })
        .await;
        d.step("register-malformed-dot", ipb, 428, |_| {
            register("bea", Some(json!({"challenge": "abcdefghijkl.mnopqrstuvwxyz", "nonce": "1"})))
        })
        .await;
        d.step("register-nonce-letters", ip, 0, |s| {
            register("bea", Some(json!({"challenge": s.v("c1"), "nonce": "12a"})))
        })
        .await;
        d.step("register-nonce-21-digits", ip, 0, |s| {
            register("bea", Some(json!({"challenge": s.v("c1"), "nonce": "1".repeat(21)})))
        })
        .await;
        d.step("register-nonce-number", ip, 0, |s| {
            register("bea", Some(json!({"challenge": s.v("c1"), "nonce": 12})))
        })
        .await;
        d.step("register-signature", ip, 428, |s| {
            let c = tampered(s, "c1");
            let nonce = crypto::solve_pow(&c, 10);
            register("bea", Some(json!({"challenge": c, "nonce": nonce})))
        })
        .await;
        d.step("register-work", ip, 428, |s| {
            let c = s.v("c1");
            register("bea", Some(json!({"challenge": c.clone(), "nonce": crypto::wrong_pow(&c, 10)})))
        })
        .await;
        d.step("register-network", fresh_ip(), 428, |s| register("bea", Some(proof(s, "c1")))).await;
        d.step("register", ip, 202, |s| register("bea", Some(proof(s, "c1")))).await;
        d.mail("register-mail", "bea@example.org", Some("bea.verify")).await;
        d.step("register-replayed", ip, 428, |s| register("cid", Some(proof(s, "c1")))).await;
        // The same address replaces the waiting signup: no conflict, a proof is needed.
        d.step("register-same-signup-no-pow", ipb, 428, |_| register("bea", None)).await;
        d.step("verify", ipb, 200, |s| Req::post("/verify-email").form(&[("token", &s.v("bea.verify"))]))
            .await;
        d.step("register-taken-no-pow", ipb, 409, |_| register("bea", None)).await;

        // The wave of failed sign-ins.
        let ip = fresh_ip();
        d.step("login-before-wave", ip, 200, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "bea", "password": PASSWORD}))
        })
        .await;
        for i in 1..=3 {
            d.step(&format!("wave-failure-{i}"), ip, 401, move |_| {
                Req::post("/api/v1/auth/login")
                    .json(json!({"login": format!("nobody{i}"), "password": "wrong password"}))
            })
            .await;
        }
        let p = d
            .step("login-wave", ip, 428, |_| {
                Req::post("/api/v1/auth/login").json(json!({"login": "bea", "password": PASSWORD}))
            })
            .await;
        save_challenge(d, &p, "l1");
        d.step("login-wave-other-address", fresh_ip(), 428, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "bea", "password": PASSWORD}))
        })
        .await;
        let p = d.step("register-challenge-for-login", ip, 428, |_| register("dan", None)).await;
        save_challenge(d, &p, "r2");
        d.step("login-endpoint", ip, 428, |s| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": "bea", "password": PASSWORD, "pow": proof(s, "r2")}))
        })
        .await;
        d.step("register-endpoint", ip, 428, |s| register("dan", Some(proof(s, "l1")))).await;
        d.step("login-wrong-password-with-pow", ip, 401, |s| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": "bea", "password": "wrong password", "pow": proof(s, "l1")}))
        })
        .await;
        let p = d
            .step("login-wave-again", ip, 428, |_| {
                Req::post("/api/v1/auth/login").json(json!({"login": "bea", "password": PASSWORD}))
            })
            .await;
        save_challenge(d, &p, "l2");
        d.step("login-with-pow", ip, 200, |s| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": "bea", "password": PASSWORD, "pow": proof(s, "l2")}))
        })
        .await;
        d.step("login-replayed", ip, 428, |s| {
            Req::post("/api/v1/auth/login")
                .json(json!({"login": "bea", "password": PASSWORD, "pow": proof(s, "l2")}))
        })
        .await;
        d.step("login-unknown-user-wave", ip, 428, |_| {
            Req::post("/api/v1/auth/login").json(json!({"login": "nobody", "password": "wrong password"}))
        })
        .await;

        // A challenge is valid for 2 minutes.
        let wait = Duration::from_secs(122).saturating_sub(issued.elapsed());
        tokio::time::sleep(wait).await;
        d.step("register-expired", ip0, 428, |s| register("ana", Some(proof(s, "cx")))).await;
    })
}
