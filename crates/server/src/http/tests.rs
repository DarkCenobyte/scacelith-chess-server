//! Pipeline tests, ported from the Node suites `http.router`, `http.quotas` and
//! `http.route-refusals` (in process, through [`TestApi`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use http::Method;
use serde_json::{Value, json};

use super::testing::{TestApi, TestBody};
use super::*;
use crate::clock::{Clock, ManualClock, SharedClock};
use crate::config::{Config, TlsMode};
use crate::log::Logger;
use crate::net::guard::IpGuard;

const TOKEN: &str = "sct_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB: &str = "sct_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Accepts [`TOKEN`] (user 7, alice) and [`BOB`] (user 8).
struct FakeAuth;

impl Authenticator for FakeAuth {
    async fn validate_token(&self, token: &str) -> Result<Option<AuthInfo>, ApiError> {
        let (id, name) = match token {
            TOKEN => (7, "alice"),
            BOB => (8, "u8"),
            _ => return Ok(None),
        };
        Ok(Some(AuthInfo {
            user_id: id,
            username: name.into(),
            session_id: 3,
            email_verified: true,
            token_hash: Some("h".into()),
        }))
    }
}

fn explode() -> Result<Answer, ApiError> {
    panic!("a handler bug")
}

fn router_under_test() -> Router {
    let mut r = Router::new();
    r.get("/echo/:name", RouteOpts::new(), |ctx: Ctx| async move {
        Ok(Answer::json(json!({"name": ctx.param("name"), "query": ctx.query, "ip": ctx.ip.to_string()})))
    });
    r.get("/echo/special", RouteOpts::new(), |_| async { Ok(Answer::json(json!({"special": true}))) });
    r.get("/api/v1/absolute", RouteOpts::new(), |_| async { Ok(Answer::json(json!({"absolute": true}))) });
    let body = Schema::new()
        .field(
            "name",
            Spec::string()
                .min_len(2)
                .max_len(8)
                .pattern(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase())),
        )
        .field("n", Spec::integer().min(1.0).max(9.0).optional())
        .field("flag", Spec::boolean().optional())
        .field("kind", Spec::one_of([json!("a"), json!("b")]).optional())
        .field("nested", Spec::object(Schema::new().field("x", Spec::string().max_len(3))).optional())
        .field("list", Spec::array(Spec::number()).max_len(2).optional())
        .field("note", Spec::string().max_len(50).optional().multiline());
    r.post(
        "/body",
        RouteOpts::new().body(body),
        |ctx: Ctx| async move { Ok(Answer::json(ctx.body).status(201)) },
    );
    r.post("/empty", RouteOpts::new(), |_| async { Ok(Answer::no_content()) });
    r.get("/private", RouteOpts::new().auth(AuthMode::Required), |ctx: Ctx| async move {
        let a = ctx.auth.expect("a session");
        Ok(Answer::json(json!({
            "user": {"id": a.user_id, "userId": a.user_id, "username": a.username, "emailVerified": a.email_verified},
            "session": {"id": a.session_id, "tokenHash": a.token_hash},
        })))
    });
    r.get("/maybe", RouteOpts::new().auth(AuthMode::Optional), |ctx: Ctx| async move {
        Ok(Answer::json(json!({"user": ctx.auth.map(|a| a.username)})))
    });
    r.get("/limited", RouteOpts::new().rate(RateSpec::new("lim", 2.0, 60000)), |_| async {
        Ok(Answer::json(json!({"ok": true})))
    });
    r.get("/shared", RouteOpts::new().rate(RateSpec::new("shr", 100.0, 60000).shared()), |_| async {
        Ok(Answer::json(json!({"ok": true})))
    });
    r.get("/boom", RouteOpts::new(), |_| async { Err(ApiError::internal("secret internal detail")) });
    r.get("/panic", RouteOpts::new(), |_| async { explode() });
    r.get("/teapot", RouteOpts::new(), |_| async {
        Err(ApiError::new(418, "teapot", "I am a teapot.").with_extra("hint", json!(42)))
    });
    r.get("/slow", RouteOpts::new(), |_| async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(Answer::json(json!({"late": true})))
    });
    r.put("/put", RouteOpts::new().body(Schema::new().field("v", Spec::number())), |ctx: Ctx| async move {
        Ok(Answer::json(ctx.body))
    });
    r.page(Method::GET, "/page", RouteOpts::new(), |_| async {
        Ok(Answer::html("<!DOCTYPE html><p>page</p>"))
    });
    r.page(
        Method::POST,
        "/page",
        RouteOpts::new().body(Schema::new().field("token", Spec::string().max_len(64))),
        |ctx: Ctx| async move { Ok(Answer::html(format!("<p>{}</p>", ctx.body["token"].as_str().unwrap_or("")))) },
    );
    r.page(Method::GET, "/page-error", RouteOpts::new(), |_| async {
        Err(ApiError::new(400, "bad", "Bad <things>."))
    });
    r.get("/file", RouteOpts::new(), |_| async {
        Ok(Answer::text("[Event \"é\"]\n\n*\n")
            .content_type("application/x-chess-pgn; charset=utf-8")
            .header("Content-Disposition", "attachment; filename=\"x.pgn\""))
    });
    r.get("/plain", RouteOpts::new(), |_| async { Ok(Answer::text("plain").status(202)) });
    r.post("/own", RouteOpts::new().own_body_validation(), |ctx: Ctx| async move {
        Ok(Answer::json(json!({"got": ctx.body})))
    });
    r
}

struct Setup {
    t: TestApi,
    clock: Arc<ManualClock>,
    guard: Option<Arc<IpGuard>>,
}

fn config_with(edit: impl FnOnce(&mut Config)) -> Config {
    let mut c = Config::for_tests();
    c.http_rate_per_ip = 100_000;
    c.http_body_limit = 1024;
    edit(&mut c);
    c
}

fn start_with(config: Config, router: Router, with_guard: bool) -> Setup {
    let clock = ManualClock::new(1_000_000.0, 1_700_000_000_000);
    let shared: SharedClock = clock.clone() as Arc<dyn Clock>;
    let config = Arc::new(config);
    let mut b = Api::builder(config.clone(), router)
        .clock(shared.clone())
        .authenticator(FakeAuth)
        .body_timeout(Duration::from_millis(150))
        .handler_timeout(Duration::from_millis(150));
    let mut guard = None;
    if with_guard {
        let g = IpGuard::new(&config, shared, Logger::root().child("test"))
            .expect("a guard")
            .with_report_sink(Box::new(|_| {}));
        let g = Arc::new(g);
        b = b.guard(g.clone());
        guard = Some(g);
    }
    Setup { t: TestApi::new(b.build()), clock, guard }
}

fn start() -> Setup {
    start_with(config_with(|_| {}), router_under_test(), false)
}

#[tokio::test]
async fn routing_params_literals_prefix_absolute_404() {
    let s = start();
    let r = s.t.get("/api/v1/echo/b%C3%A9b%C3%A9?x=1&x=2&y=z").send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"name": "bébé", "query": {"x": "1", "y": "z"}, "ip": "203.0.113.10"}));
    assert_eq!(s.t.get("/api/v1/echo/special").send().await.json(), json!({"special": true}));
    assert_eq!(s.t.get("/api/v1/echo/special/").send().await.json(), json!({"special": true}));
    assert_eq!(s.t.get("/api/v1/absolute").send().await.json(), json!({"absolute": true}));
    let r = s.t.get("/echo/x").send().await;
    assert_eq!((r.status, r.json()), (404, json!({"error": "not_found", "message": "No such endpoint."})));
    let r = s.t.get("/api/v1/echo/%E0%A4%A").send().await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "invalid_request", "message": "malformed path"})));
    let r = s.t.get("//evil/x").send().await;
    assert_eq!((r.status, r.json()["message"].clone()), (400, json!("Invalid request target.")));
    let r = s.t.get("http://example.com/api/v1/echo/x").send().await;
    assert_eq!(r.status, 400, "absolute form");
    let r = s.t.request(Method::OPTIONS, "*").send().await;
    assert_eq!(r.status, 400, "asterisk form");
    let long = format!("/api/v1/echo/{}", "a".repeat(4096));
    let r = s.t.get(&long).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (414, json!("uri_too_long")));
}

#[tokio::test]
async fn method_not_allowed_options_and_head() {
    let s = start();
    let r = s.t.request(Method::DELETE, "/api/v1/echo/x").send().await;
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, HEAD, OPTIONS")));
    assert_eq!(r.json()["error"], "method_not_allowed");
    let r =
        s.t.request(Method::OPTIONS, "/api/v1/body")
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "POST")
            .send()
            .await;
    assert_eq!((r.status, r.header("allow")), (204, Some("POST, OPTIONS")));
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!((r.header("content-type"), r.header("content-length")), (None, None));
    let r = s.t.request(Method::HEAD, "/api/v1/echo/x").send().await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    assert!(r.header("content-length").expect("a length").parse::<u32>().expect("a number") > 0);
    let r = s.t.request(Method::HEAD, "/api/v1/body").send().await;
    assert_eq!((r.status, r.header("allow")), (405, Some("POST, OPTIONS")));
    assert!(r.body.is_empty() && r.header("content-length").is_some());
}

#[tokio::test]
async fn security_headers_everywhere_hsts_with_native_tls_only() {
    let s = start();
    for path in ["/api/v1/echo/x", "/nope", "/page"] {
        let r = s.t.get(path).send().await;
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
        assert!(r.header("content-security-policy").is_some());
        assert_eq!(r.header("strict-transport-security"), None);
    }
    let page = s.t.get("/page").send().await;
    assert_eq!(page.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(page.header("content-security-policy"), Some(api::PAGE_CSP));
    let r = s.t.get("/api/v1/echo/x").send().await;
    assert_eq!(
        r.header_names(),
        [
            "cache-control",
            "x-content-type-options",
            "referrer-policy",
            "x-frame-options",
            "cross-origin-resource-policy",
            "content-security-policy",
            "content-type",
            "content-length"
        ]
    );
    let n = start_with(config_with(|c| c.tls_mode = TlsMode::Native), router_under_test(), false);
    for path in ["/api/v1/echo/x", "/api/v1/file", "/nope"] {
        assert_eq!(n.t.get(path).send().await.header("strict-transport-security"), Some("max-age=31536000"));
    }
}

#[tokio::test]
async fn own_body_validation_passes_any_json() {
    let s = start();
    let body = json!({"gameId": "123", "anything": [1, {"x": null}]});
    let r = s.t.post("/api/v1/own").json(&body).send().await;
    assert_eq!((r.status, r.json()["got"].clone()), (200, body));
    assert_eq!(s.t.post("/api/v1/own").json(&json!([1, 2])).send().await.json()["got"], json!([1, 2]));
    assert_eq!(
        s.t.post("/api/v1/own").send().await.json()["got"],
        json!({}),
        "an empty body is an empty object"
    );
    assert_eq!(
        s.t.post("/api/v1/own").body("application/json", "{bad").send().await.json()["error"],
        "invalid_json"
    );
    assert_eq!(
        s.t.post("/api/v1/own").body("application/x-www-form-urlencoded", "a=1").send().await.status,
        415
    );
    let big = format!("{{\"x\":\"{}\"}}", "y".repeat(2000));
    assert_eq!(s.t.post("/api/v1/own").body("application/json", big).send().await.status, 413);
    let r = s.t.post("/api/v1/empty").json(&json!({"gameId": 1})).send().await;
    assert_eq!((r.status, r.json()["field"].clone()), (400, json!("gameId")));
}

#[tokio::test]
async fn text_answers() {
    let s = start();
    let text = "[Event \"é\"]\n\n*\n";
    let r = s.t.get("/api/v1/file").send().await;
    assert_eq!((r.status, r.text()), (200, text));
    assert_eq!(r.header("content-type"), Some("application/x-chess-pgn; charset=utf-8"));
    assert_eq!(r.header("content-disposition"), Some("attachment; filename=\"x.pgn\""));
    assert_eq!(r.header("content-length"), Some(text.len().to_string().as_str()));
    assert_eq!(r.header("content-security-policy"), Some(api::API_CSP));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    let r = s.t.get("/api/v1/plain").send().await;
    assert_eq!(
        (r.status, r.text(), r.header("content-type")),
        (202, "plain", Some("text/plain; charset=utf-8"))
    );
    let r = s.t.request(Method::HEAD, "/api/v1/file").send().await;
    assert_eq!((r.status, r.body.len()), (200, 0));
    assert_eq!(r.header("content-length"), Some(text.len().to_string().as_str()));
}

#[tokio::test]
async fn json_bodies() {
    let s = start();
    let full = json!({"name": "abc", "n": 3, "flag": true, "kind": "b", "nested": {"x": "yz"}, "list": [1, 2.5], "note": "two\nlines"});
    let r = s.t.post("/api/v1/body").json(&full).send().await;
    assert_eq!((r.status, r.json()), (201, full));
    let r = s.t.post("/api/v1/body").body("application/x-www-form-urlencoded", "name=abc").send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (415, json!("unsupported_media_type")));
    let r = s.t.post("/api/v1/body").body("text/plain", "{\"name\":\"abc\"}").send().await;
    assert_eq!(r.status, 415, "simple cross-site requests are refused");
    let r =
        s.t.post("/api/v1/body").body("application/json; charset=latin1", "{\"name\":\"abc\"}").send().await;
    assert_eq!(r.status, 415);
    let r =
        s.t.post("/api/v1/body").body("application/json; charset=UTF-8", "{\"name\":\"abc\"}").send().await;
    assert_eq!(r.status, 201);
    let r = s.t.post("/api/v1/body").body("application/json", "{\"name\":").send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (400, json!("invalid_json")));
    let r = s.t.post("/api/v1/body").send().await;
    assert_eq!(
        (r.status, r.json()["error"].clone(), r.json()["field"].clone()),
        (400, json!("invalid_request"), json!("name"))
    );
    assert_eq!(s.t.post("/api/v1/empty").send().await.status, 204);
    assert_eq!(s.t.post("/api/v1/empty").json(&json!({"x": 1})).send().await.status, 400);
    let r = s.t.request(Method::PUT, "/api/v1/put").json(&json!({"v": 1.5})).send().await;
    assert_eq!(r.json(), json!({"v": 1.5}));
}

#[tokio::test]
async fn strict_schema_validation() {
    let s = start();
    let bad = [
        (json!({"name": "abc", "extra": 1}), "extra"),
        (json!({"name": "a"}), "name"),
        (json!({"name": "abcdefghi"}), "name"),
        (json!({"name": "ABC"}), "name"),
        (json!({"name": 5}), "name"),
        (json!({"name": "abc", "n": 1.5}), "n"),
        (json!({"name": "abc", "n": 10}), "n"),
        (json!({"name": "abc", "n": "3"}), "n"),
        (json!({"name": "abc", "flag": "yes"}), "flag"),
        (json!({"name": "abc", "kind": "c"}), "kind"),
        (json!({"name": "abc", "nested": {"x": "long"}}), "nested.x"),
        (json!({"name": "abc", "nested": {"y": 1}}), "nested.y"),
        (json!({"name": "abc", "nested": []}), "nested"),
        (json!({"name": "abc", "list": [1, 2, 3]}), "list"),
        (json!({"name": "abc", "list": ["x"]}), "list[0]"),
        (json!({"name": "ab\u{0}c"}), "name"),
        (json!({"name": "abc", "note": "bell\u{7}"}), "note"),
        (json!({"name": null}), "name"),
    ];
    for (body, field) in bad {
        let r = s.t.post("/api/v1/body").json(&body).send().await;
        assert_eq!(r.status, 400, "{body}");
        assert_eq!(r.json()["error"], "invalid_request");
        assert_eq!(r.json()["field"], field, "{body}");
    }
    let r =
        s.t.post("/api/v1/body")
            .body("application/json", "{\"__proto__\":{\"admin\":true},\"name\":\"abc\"}")
            .send()
            .await;
    assert_eq!(r.status, 400);
    let r = s.t.post("/api/v1/body").body("application/json", "[1]").send().await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json().get("field"), None, "the body itself has no field");
}

#[tokio::test]
async fn body_limit_from_content_length_and_streaming() {
    let s = start();
    let big = format!("{{\"name\":\"{}\"}}", "a".repeat(2000));
    let r = s.t.post("/api/v1/body").body("application/json", big).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (413, json!("payload_too_large")));
    assert_eq!(r.header("connection"), Some("close"));
    let (xs, ys) = ("x".repeat(600), "y".repeat(600));
    let chunks = ["{\"name\":\"", &xs, &ys, "\"}"];
    let r =
        s.t.post("/api/v1/body")
            .header("content-type", "application/json")
            .raw_body(TestBody::chunks(&chunks))
            .send()
            .await;
    assert_eq!((r.status, r.header("connection")), (413, Some("close")));
    let r =
        s.t.post("/api/v1/body")
            .header("content-type", "application/json")
            .header("content-length", "12x")
            .send()
            .await;
    assert_eq!((r.status, r.json()["message"].clone()), (400, json!("Invalid Content-Length.")));
}

#[tokio::test(start_paused = true)]
async fn slow_bodies_408_slow_handlers_503() {
    let s = start();
    let (tx, body) = TestBody::channel();
    tx.send("{\"name\":");
    let r =
        s.t.post("/api/v1/body")
            .header("content-type", "application/json")
            .header("content-length", "100")
            .raw_body(body)
            .send()
            .await;
    assert_eq!(
        (r.status, r.json()["error"].clone(), r.header("connection")),
        (408, json!("request_timeout"), Some("close"))
    );
    drop(tx);
    let r = s.t.get("/api/v1/slow").send().await;
    assert_eq!(
        (r.status, r.json()),
        (503, json!({"error": "timeout", "message": "The server took too long to answer; try again."}))
    );
}

#[tokio::test]
async fn the_shutdown_waits_for_the_late_handlers() {
    let s = start();
    assert!(s.t.api.quiesce(Duration::ZERO).await, "nothing runs");
    let r = s.t.get("/api/v1/slow").send().await;
    assert_eq!(r.status, 503, "answered at the route timeout");
    assert_eq!(s.t.api.handlers_running(), 1, "the late handler still runs");
    assert!(!s.t.api.quiesce(Duration::from_millis(10)).await);
    assert!(s.t.api.quiesce(Duration::from_secs(5)).await);
    assert_eq!(s.t.api.handlers_running(), 0);
}

#[tokio::test]
async fn authentication_modes() {
    let s = start();
    let r = s.t.get("/api/v1/private").send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("unauthorized")));
    assert_eq!(r.header("www-authenticate"), Some("Bearer realm=\"scacelith\""));
    let r = s.t.get("/api/v1/private").bearer(&format!("sct_{}", "c".repeat(43))).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (401, json!("invalid_token")));
    assert_eq!(r.header("www-authenticate"), Some("Bearer realm=\"scacelith\", error=\"invalid_token\""));
    assert_eq!(s.t.get("/api/v1/private").header("authorization", "Basic abc").send().await.status, 401);
    assert_eq!(
        s.t.get("/api/v1/private").header("authorization", &format!("bearer {TOKEN}")).send().await.status,
        401
    );
    assert_eq!(
        s.t.get("/api/v1/private").header("authorization", &format!("Bearer  {TOKEN}")).send().await.status,
        401
    );
    let r = s.t.get("/api/v1/private").bearer(TOKEN).send().await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.json(),
        json!({"user": {"id": 7, "userId": 7, "username": "alice", "emailVerified": true}, "session": {"id": 3, "tokenHash": "h"}})
    );
    assert_eq!(s.t.get("/api/v1/maybe").send().await.json(), json!({"user": null}));
    assert_eq!(
        s.t.get("/api/v1/maybe").header("authorization", "").send().await.json(),
        json!({"user": null})
    );
    assert_eq!(s.t.get("/api/v1/maybe").bearer(TOKEN).send().await.json(), json!({"user": "alice"}));
    assert_eq!(s.t.get("/api/v1/maybe").bearer("nope").send().await.status, 401);
    assert_eq!(
        s.t.get("/api/v1/echo/x").bearer("nope").send().await.status,
        200,
        "auth none ignores the header"
    );
}

#[tokio::test]
async fn route_rates_local_and_shared() {
    let s = start();
    assert_eq!(s.t.get("/api/v1/limited").send().await.status, 200);
    assert_eq!(s.t.get("/api/v1/limited").send().await.status, 200);
    let r = s.t.get("/api/v1/limited").send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    assert_eq!(r.header("retry-after"), Some(r.json()["retryAfter"].to_string().as_str()));
    assert_eq!(r.json()["retryAfter"], 30);
    let c = s.t.clone().at("2001:db8::1");
    assert_eq!(c.get("/api/v1/shared").send().await.status, 200);
    let shared = s.t.api.rates().shared().clone();
    assert_eq!(shared.peek("shr:2001:db8:0:0::/64"), 1.0);
    // Fill the shared window behind the local bucket's back: the shared stage refuses.
    for _ in 0..99 {
        shared.take("shr:2001:db8:0:0::/64", 100.0, 60000, 1.0);
    }
    let r = c.get("/api/v1/shared").send().await;
    assert_eq!(r.status, 429);
    let wait = r.json()["retryAfter"].as_u64().expect("seconds");
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(r.header("retry-after"), Some(wait.to_string().as_str()));
    s.clock.advance(60_000.0);
    assert_eq!(c.get("/api/v1/shared").send().await.status, 200);
}

#[tokio::test]
async fn errors_uniform_json_hidden_internals_html_on_pages() {
    let s = start();
    let r = s.t.get("/api/v1/boom").send().await;
    assert_eq!(
        (r.status, r.json()),
        (500, json!({"error": "internal_error", "message": "Internal server error."}))
    );
    assert!(!r.text().contains("secret"));
    let r = s.t.get("/api/v1/panic").send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (500, json!("internal_error")));
    let r = s.t.get("/api/v1/teapot").send().await;
    assert_eq!(
        (r.status, r.json()),
        (418, json!({"error": "teapot", "message": "I am a teapot.", "hint": 42}))
    );
    let r = s.t.get("/page-error").send().await;
    assert_eq!(r.status, 400);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    assert!(r.text().contains("Bad &lt;things&gt;."), "{}", r.text());
    assert!(r.text().contains("Request refused"));
}

#[tokio::test]
async fn page_forms() {
    let s = start();
    let form = "application/x-www-form-urlencoded";
    let r = s.t.post("/page").body(form, "token=abc%2Bdef").send().await;
    assert_eq!((r.status, r.text()), (200, "<p>abc+def</p>"));
    let r = s.t.post("/page").body(form, "token=a&token=b").send().await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("field &quot;token&quot; given twice"), "{}", r.text());
    assert_eq!(s.t.post("/page").body(form, "token=a&x=1").send().await.status, 400);
    assert_eq!(s.t.post("/page").body("text/plain", "token=a").send().await.status, 415);
}

#[tokio::test]
async fn health_and_readiness() {
    let ready = crate::net::health::Readiness::new();
    let api =
        Api::builder(Arc::new(Config::for_tests()), router_under_test()).readiness(ready.clone()).build();
    let t = TestApi::new(api);
    assert_eq!(t.get("/healthz").send().await.json(), json!({"status": "ok"}));
    let r = t.get("/api/v1/healthz").send().await;
    assert_eq!(
        (r.json(), r.header("content-type")),
        (json!({"status": "ok"}), Some("application/json; charset=utf-8"))
    );
    let r = t.get("/readyz").send().await;
    assert_eq!((r.status, r.json()), (503, json!({"status": "not_ready"})));
    ready.set(true);
    assert_eq!(t.get("/readyz").send().await.status, 200);
    let r = t.post("/healthz").send().await;
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, HEAD")));
    assert_eq!(t.request(Method::OPTIONS, "/api/v1/readyz").send().await.status, 405);
    assert_eq!(t.get("/healthz/").send().await.status, 404, "no trailing slash on the health paths");
}

fn quota_routes(closed: Arc<parking_lot::Mutex<Vec<&'static str>>>) -> Router {
    let mut r = Router::new();
    r.get("/a", RouteOpts::new().auth(AuthMode::Optional), |_| async {
        Ok(Answer::json(json!({"a": true})))
    });
    r.get("/b", RouteOpts::new().auth(AuthMode::Required), |_| async {
        Ok(Answer::json(json!({"b": true})))
    });
    r.get(
        "/byuser",
        RouteOpts::new().auth(AuthMode::Optional).rate(RateSpec::new("pr", 2.0, 60000).by_user()),
        |ctx: Ctx| async move { Ok(Answer::json(json!({"user": ctx.user_id()}))) },
    );
    r.get(
        "/two",
        RouteOpts::new().rate(RateSpec::new("slow", 2.0, 3_600_000)).rate(RateSpec::new("fast", 1.0, 60000)),
        |_| async { Ok(Answer::json(json!({"ok": true}))) },
    );
    r.get(
        "/take",
        RouteOpts::new().auth(AuthMode::Required).rate(RateSpec::new("route", 1.0, 3_600_000).by_user()),
        |ctx: Ctx| async move {
            ctx.take_rates(&[RateSpec::new("extra", 1.0, 3_600_000).shared()])?;
            if ctx.query_str("refund").is_some() {
                return Ok(Answer::json(json!({"error": "server_busy", "message": "busy", "retryAfter": 2}))
                    .status(503)
                    .header("Retry-After", "2")
                    .refund_rate());
            }
            Ok(Answer::json(json!({"ok": true})))
        },
    );
    r.get("/bin", RouteOpts::new(), |ctx: Ctx| async move {
        let status = ctx.query_str("s").and_then(|s| s.parse().ok()).unwrap_or(200);
        Ok(Answer::bytes(vec![0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0, 1, 2])
            .content_type("image/gif")
            .header("Content-Disposition", "attachment; filename=\"x.gif\"")
            .status(status))
    });
    r.get("/bin-default", RouteOpts::new(), |_| async { Ok(Answer::bytes(vec![1, 2, 3])) });
    r.post(
        "/big-body",
        RouteOpts::new().body(Schema::new().field("s", Spec::string().max_len(100_000))).body_limit(4096),
        |ctx: Ctx| async move { Ok(Answer::json(json!({"n": ctx.body["s"].as_str().map_or(0, str::len)}))) },
    );
    r.on_close(move || async move { closed.lock().push("quota routes") });
    r
}

fn quotas(edit: impl FnOnce(&mut Config)) -> Setup {
    start_with(config_with(edit), quota_routes(Default::default()), false)
}

#[tokio::test]
async fn the_account_budget() {
    let s = quotas(|c| c.user_rate_per_min = 8);
    for p in ["/a", "/b", "/a", "/b"] {
        assert_eq!(s.t.get(&format!("/api/v1{p}")).bearer(TOKEN).send().await.status, 200, "{p}");
    }
    let r = s.t.get("/api/v1/a").bearer(TOKEN).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (429, json!("rate_limited")));
    assert_eq!(r.header("retry-after"), Some(r.json()["retryAfter"].to_string().as_str()));
    assert_eq!(
        s.t.clone().at("198.51.100.9").get("/api/v1/b").bearer(TOKEN).send().await.status,
        429,
        "any address"
    );
    assert_eq!(s.t.get("/api/v1/a").send().await.status, 200, "anonymous requests are not counted");
    assert_eq!(s.t.get("/api/v1/a").bearer(BOB).send().await.status, 200, "another account");
    let other = format!("sct_{}", "z".repeat(43));
    assert_eq!(
        s.t.get("/api/v1/b").bearer(&other).send().await.status,
        401,
        "an invalid token is refused first"
    );
    s.clock.advance(7500.0);
    assert_eq!(s.t.get("/api/v1/a").bearer(TOKEN).send().await.status, 200, "one token every 7.5 s");
    assert_eq!(s.t.get("/api/v1/a").bearer(TOKEN).send().await.status, 429);
}

#[tokio::test]
async fn by_user_limits() {
    let s = quotas(|_| {});
    let at = |ip: &str| s.t.clone().at(ip);
    let ip = "192.0.2.77";
    assert_eq!(at(ip).get("/api/v1/byuser").send().await.status, 200);
    assert_eq!(at(ip).get("/api/v1/byuser").send().await.status, 200);
    assert_eq!(at(ip).get("/api/v1/byuser").send().await.status, 429, "anonymous: the address");
    assert_eq!(at(ip).get("/api/v1/byuser").bearer(TOKEN).send().await.json()["user"], 7);
    assert_eq!(at("192.0.2.78").get("/api/v1/byuser").bearer(TOKEN).send().await.status, 200);
    assert_eq!(at("192.0.2.79").get("/api/v1/byuser").bearer(TOKEN).send().await.status, 429, "any address");
    assert_eq!(at(ip).get("/api/v1/byuser").bearer(BOB).send().await.status, 200, "another account");
}

#[tokio::test]
async fn a_later_refusal_gives_back_the_earlier_tokens() {
    let s = quotas(|_| {});
    assert_eq!(s.t.get("/api/v1/two").send().await.status, 200);
    for _ in 0..4 {
        assert_eq!(s.t.get("/api/v1/two").send().await.status, 429, "refused by fast");
    }
    s.clock.advance(60000.0);
    assert_eq!(s.t.get("/api/v1/two").send().await.status, 200, "slow kept its token");
    s.clock.advance(60000.0);
    assert_eq!(s.t.get("/api/v1/two").send().await.status, 429, "now slow is spent");
}

#[tokio::test]
async fn take_rates_and_refunds() {
    let s = quotas(|_| {});
    let r = s.t.get("/api/v1/take?refund=1").bearer(TOKEN).send().await;
    assert_eq!(
        (r.status, r.json()["error"].clone(), r.header("retry-after")),
        (503, json!("server_busy"), Some("2"))
    );
    assert_eq!(s.t.api.rates().shared().peek("extra:203.0.113.10"), 0.0, "the shared rate is refunded");
    let r = s.t.get("/api/v1/take").bearer(TOKEN).send().await;
    assert_eq!(r.status, 200, "both the route's token and the handler's were given back");
    let r = s.t.get("/api/v1/take").bearer(TOKEN).send().await;
    assert_eq!(r.status, 429, "the route limit (1 per hour per account)");
    let r = s.t.get("/api/v1/take").bearer(BOB).send().await;
    assert_eq!(r.status, 429, "the handler's rate (1 per hour per address)");
    let b = s.t.clone().at("198.51.100.3");
    assert_eq!(b.get("/api/v1/take").bearer(BOB).send().await.status, 429, "the route's token stayed spent");
    s.clock.advance(3_601_000.0);
    assert_eq!(b.get("/api/v1/take").bearer(BOB).send().await.status, 200);
}

#[tokio::test]
async fn binary_answers() {
    let s = quotas(|_| {});
    let r = s.t.get("/api/v1/bin").send().await;
    assert_eq!((r.status, &r.body[..]), (200, &[0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0, 1, 2][..]));
    assert_eq!(r.header("content-type"), Some("image/gif"));
    assert_eq!(r.header("content-length"), Some("9"));
    assert_eq!(r.header("content-disposition"), Some("attachment; filename=\"x.gif\""));
    assert_eq!(r.header("content-security-policy"), Some(api::API_CSP));
    let r = s.t.request(Method::HEAD, "/api/v1/bin").send().await;
    assert_eq!((r.status, r.body.len(), r.header("content-length")), (200, 0, Some("9")));
    assert_eq!(
        s.t.get("/api/v1/bin-default").send().await.header("content-type"),
        Some("application/octet-stream")
    );
    let r = s.t.get("/api/v1/bin?s=204").send().await;
    assert_eq!(
        (r.status, r.body.len(), r.header("content-length"), r.header("content-type")),
        (204, 0, None, None)
    );
}

#[tokio::test]
async fn a_route_body_limit_replaces_the_default() {
    let s = quotas(|c| c.http_body_limit = 1024);
    let r = s.t.post("/api/v1/big-body").json(&json!({"s": "x".repeat(3000)})).send().await;
    assert_eq!((r.status, r.json()["n"].clone()), (200, json!(3000)));
    let r = s.t.post("/api/v1/big-body").json(&json!({"s": "x".repeat(5000)})).send().await;
    assert_eq!((r.status, r.json()["error"].clone()), (413, json!("payload_too_large")));
}

#[tokio::test]
async fn close_runs_the_hooks_once() {
    let closed: Arc<parking_lot::Mutex<Vec<&'static str>>> = Default::default();
    let s = start_with(config_with(|_| {}), quota_routes(closed.clone()), false);
    s.t.api.close().await;
    s.t.api.close().await;
    assert_eq!(*closed.lock(), ["quota routes"]);
}

fn refusal_routes() -> Router {
    let ok = |_| async { Ok(Answer::json(json!({"ok": true}))) };
    let mut r = Router::new();
    r.get("/ip", RouteOpts::new().rate(RateSpec::new("ipr", 1.0, 60000)), ok);
    r.post(
        "/login",
        RouteOpts::new()
            .rate(RateSpec::new("auth", 1.0, 600_000).shared().prefix_limit(5.0).abuse_weight(5.0)),
        ok,
    );
    r.post(
        "/v6",
        RouteOpts::new().rate(RateSpec::new("v6", 5.0, 600_000).prefix_limit(1.0).abuse_weight(5.0)),
        ok,
    );
    r.get("/remote", RouteOpts::new().rate(RateSpec::new("remote", 100.0, 60000).shared()), ok);
    r.get(
        "/mine",
        RouteOpts::new().auth(AuthMode::Optional).rate(RateSpec::new("pub", 1.0, 60000).by_user()),
        ok,
    );
    r.get("/busy", RouteOpts::new(), |_| async {
        Err(ApiError::new(429, "rate_limited", "Busy.").with_extra("retryAfter", json!(1)).refund_rate())
    });
    r
}

fn counted(s: &Setup) -> HashMap<String, f64> {
    let guard = s.guard.as_ref().expect("a guard");
    guard.flush_reports().into_iter().map(|e| (e.k64.to_string(), e.weight)).collect()
}

fn weights(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
    pairs.iter().map(|(k, w)| (k.to_string(), *w)).collect()
}

#[tokio::test]
async fn address_keyed_refusals_count_toward_a_block() {
    let s = start_with(config_with(|_| {}), refusal_routes(), true);
    let at = |ip: &str| s.t.clone().at(ip);
    assert_eq!(at("198.51.100.1").get("/api/v1/ip").send().await.status, 200);
    assert_eq!(at("198.51.100.1").get("/api/v1/ip").send().await.status, 429);
    assert_eq!(at("198.51.100.1").get("/api/v1/ip").send().await.status, 429);
    assert_eq!(at("198.51.100.2").post("/api/v1/login").send().await.status, 200);
    assert_eq!(at("198.51.100.2").post("/api/v1/login").send().await.status, 429, "the auth family");
    assert_eq!(counted(&s), weights(&[("198.51.100.1", 2.0), ("198.51.100.2", 5.0)]));
}

#[tokio::test]
async fn a_shared_window_refusal_counts_like_a_local_one() {
    let s = start_with(config_with(|_| {}), refusal_routes(), true);
    let shared = s.t.api.rates().shared().clone();
    for _ in 0..100 {
        shared.take("remote:198.51.100.4", 100.0, 60000, 1.0);
    }
    assert_eq!(s.t.clone().at("198.51.100.4").get("/api/v1/remote").send().await.status, 429);
    shared.take("auth:198.51.100.5", 1.0, 600_000, 1.0);
    assert_eq!(s.t.clone().at("198.51.100.5").post("/api/v1/login").send().await.status, 429);
    assert_eq!(counted(&s), weights(&[("198.51.100.4", 1.0), ("198.51.100.5", 5.0)]));
}

#[tokio::test]
async fn a_48_sub_limit_counts_against_the_asking_64() {
    let s = start_with(config_with(|_| {}), refusal_routes(), true);
    assert_eq!(s.t.clone().at("2001:db8:4:1::1").post("/api/v1/v6").send().await.status, 200);
    assert_eq!(s.t.clone().at("2001:db8:4:2::1").post("/api/v1/v6").send().await.status, 429);
    assert_eq!(counted(&s), weights(&[("2001:db8:4:2::/64", 5.0)]));
}

#[tokio::test]
async fn account_keyed_refusals_never_count() {
    let s = start_with(config_with(|_| {}), refusal_routes(), true);
    let c = s.t.clone().at("198.51.100.6");
    assert_eq!(c.get("/api/v1/mine").bearer(TOKEN).send().await.status, 200);
    assert_eq!(c.get("/api/v1/mine").bearer(TOKEN).send().await.status, 429);
    assert_eq!(c.get("/api/v1/mine").bearer(TOKEN).send().await.status, 429);
    assert!(counted(&s).is_empty(), "an account's problem, not its network's");
    let anon = s.t.clone().at("198.51.100.7");
    assert_eq!(anon.get("/api/v1/mine").send().await.status, 200);
    assert_eq!(anon.get("/api/v1/mine").send().await.status, 429);
    assert_eq!(counted(&s), weights(&[("198.51.100.7", 1.0)]));
}

#[tokio::test]
async fn other_429s_are_not_counted_and_nothing_without_a_guard() {
    let s = start_with(config_with(|_| {}), refusal_routes(), true);
    for _ in 0..3 {
        let r = s.t.clone().at("198.51.100.8").get("/api/v1/busy").send().await;
        assert_eq!((r.status, r.header("retry-after")), (429, Some("1")));
    }
    assert!(counted(&s).is_empty());
    let bare = start_with(config_with(|_| {}), refusal_routes(), false);
    assert_eq!(bare.t.get("/api/v1/ip").send().await.status, 200);
    assert_eq!(bare.t.get("/api/v1/ip").send().await.status, 429, "the limit itself still applies");
}

#[tokio::test]
async fn query_schemas_always_name_the_field() {
    let mut r = Router::new();
    r.get(
        "/q",
        RouteOpts::new().query(Schema::new().field("n", Spec::string().max_len(2))),
        |ctx: Ctx| async move { Ok(Answer::json(Value::Object(ctx.query))) },
    );
    let t = TestApi::from_router(r);
    assert_eq!(t.get("/api/v1/q?n=ab").send().await.json(), json!({"n": "ab"}));
    let res = t.get("/api/v1/q?n=abc").send().await;
    assert_eq!((res.status, res.json()["field"].clone()), (400, json!("n")));
    let res = t.get("/api/v1/q?x=1").send().await;
    assert_eq!(
        res.json(),
        json!({"error": "invalid_request", "message": "unknown field \"x\"", "field": "x"})
    );
}
