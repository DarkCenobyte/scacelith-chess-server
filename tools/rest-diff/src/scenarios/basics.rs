//! Server info, health endpoints, routing: 404, 405 with `Allow`, `OPTIONS`, `HEAD`, trailing
//! slashes, query strings, URL decoding of path parameters, the request-target checks.

use super::BoxFut;
use crate::duo::{Duo, fresh_ip};
use crate::http::Req;

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        let ip = fresh_ip();
        d.step("info", ip, 200, |_| Req::get("/api/v1/info")).await;
        d.step("info-trailing-slash", ip, 200, |_| Req::get("/api/v1/info/")).await;
        d.step("info-query-ignored", ip, 200, |_| Req::get("/api/v1/info?x=1&x=2&&=&y")).await;
        d.step("info-head", ip, 200, |_| Req::new("HEAD", "/api/v1/info")).await;
        d.step("info-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/info")).await;
        d.step("info-post", ip, 405, |_| Req::post("/api/v1/info")).await;
        d.step("info-put-json", ip, 405, |_| Req::new("PUT", "/api/v1/info").json(serde_json::json!({}))).await;
        d.step("info-delete", ip, 405, |_| Req::new("DELETE", "/api/v1/info")).await;
        d.step("info-patch", ip, 405, |_| Req::new("PATCH", "/api/v1/info")).await;
        d.step("info-double-trailing-slash", ip, 404, |_| Req::get("/api/v1/info//")).await;
        d.step("info-encoded-literal", ip, 404, |_| Req::get("/api/v1/%69nfo")).await;
        d.step("info-upper-case", ip, 404, |_| Req::get("/API/V1/INFO")).await;
        d.step("info-dot-segment", ip, 404, |_| Req::get("/api/v1/x/../info")).await;

        for path in ["/healthz", "/readyz", "/api/v1/healthz", "/api/v1/readyz"] {
            let name = path.trim_start_matches('/').replace('/', "-");
            d.step(&format!("{name}-get"), ip, 200, |_| Req::get(path)).await;
            d.step(&format!("{name}-head"), ip, 200, |_| Req::new("HEAD", path)).await;
            d.step(&format!("{name}-post"), ip, 405, |_| Req::post(path)).await;
            d.step(&format!("{name}-options"), ip, 405, |_| Req::new("OPTIONS", path)).await;
        }
        d.step("healthz-query", ip, 200, |_| Req::get("/healthz?full=1")).await;
        d.step("healthz-trailing-slash", ip, 404, |_| Req::get("/healthz/")).await;

        d.step("root", ip, 404, |_| Req::get("/")).await;
        d.step("api-prefix", ip, 404, |_| Req::get("/api/v1")).await;
        d.step("api-prefix-slash", ip, 404, |_| Req::get("/api/v1/")).await;
        d.step("unknown-api", ip, 404, |_| Req::get("/api/v1/nope")).await;
        d.step("unknown-page", ip, 404, |_| Req::get("/nope")).await;
        d.step("unknown-options", ip, 404, |_| Req::new("OPTIONS", "/api/v1/nope")).await;
        d.step("unknown-post", ip, 404, |_| Req::post("/api/v1/nope").json(serde_json::json!({}))).await;
        d.step("ws-path-get", ip, 0, |_| Req::get("/ws")).await;
        d.step("api-v2", ip, 404, |_| Req::get("/api/v2/info")).await;

        // Methods of paths with several routes (Allow lists them in route order).
        d.step("account-preferences-get", ip, 405, |_| Req::get("/api/v1/account/preferences")).await;
        d.step("account-preferences-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/account/preferences")).await;
        d.step("account-me-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/account/me")).await;
        d.step("sessions-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/auth/sessions")).await;
        d.step("session-id-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/auth/sessions/1")).await;
        d.step("session-id-get", ip, 405, |_| Req::get("/api/v1/auth/sessions/1")).await;
        d.step("verify-email-options", ip, 204, |_| Req::new("OPTIONS", "/verify-email")).await;
        d.step("verify-email-put", ip, 405, |_| Req::new("PUT", "/verify-email")).await;
        d.step("reset-password-delete", ip, 405, |_| Req::new("DELETE", "/reset-password")).await;
        d.step("gif-get", ip, 405, |_| Req::get("/api/v1/gif")).await;
        d.step("games-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/games/1")).await;
        d.step("games-pgn-post", ip, 405, |_| Req::post("/api/v1/games/1/pgn")).await;
        d.step("games-gif-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/games/1/gif")).await;
        d.step("players-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/players/alice")).await;
        d.step("players-games-delete", ip, 405, |_| Req::new("DELETE", "/api/v1/players/alice/games")).await;
        d.step("leaderboard-post", ip, 405, |_| Req::post("/api/v1/leaderboard")).await;
        d.step("reports-get", ip, 405, |_| Req::get("/api/v1/reports")).await;
        d.step("login-get", ip, 405, |_| Req::get("/api/v1/auth/login")).await;
        d.step("login-head", ip, 405, |_| Req::new("HEAD", "/api/v1/auth/login")).await;
        d.step("login-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/auth/login")).await;
        d.step("sso-start-options", ip, 204, |_| Req::new("OPTIONS", "/api/v1/auth/sso/google/start")).await;

        // Path parameters: empty segments, decoding, invalid encodings.
        d.step("players-empty-segment", ip, 404, |_| Req::get("/api/v1/players//games")).await;
        d.step("players-encoded", ip, 404, |_| Req::get("/api/v1/players/al%69ce")).await;
        d.step("players-bad-encoding", ip, 400, |_| Req::get("/api/v1/players/%E0%A4%A")).await;
        d.step("players-bad-encoding-percent", ip, 400, |_| Req::get("/api/v1/players/abc%")).await;
        d.step("players-encoded-slash", ip, 400, |_| Req::get("/api/v1/players/a%2Fb")).await;
        d.step("players-plus", ip, 404, |_| Req::get("/api/v1/players/a+b")).await;
        d.step("games-bad-encoding", ip, 400, |_| Req::get("/api/v1/games/%ZZ")).await;
        d.step("session-bad-encoding-unauth", ip, 0, |_| Req::new("DELETE", "/api/v1/auth/sessions/%ZZ")).await;

        // The request target.
        let long_query = format!("/api/v1/info?q={}", "a".repeat(4100));
        d.step("uri-too-long", fresh_ip(), 414, |_| Req::get(long_query.clone())).await;
        let exact = format!("/api/v1/info?q={}", "a".repeat(4096 - "/api/v1/info?q=".len()));
        d.step("uri-4096", ip, 200, |_| Req::get(exact.clone())).await;
        let over = format!("/api/v1/info?q={}", "a".repeat(4097 - "/api/v1/info?q=".len()));
        d.step("uri-4097", fresh_ip(), 414, |_| Req::get(over.clone())).await;
        let long_path = format!("/api/v1/{}", "p".repeat(5000));
        d.step("uri-long-path", fresh_ip(), 414, |_| Req::get(long_path.clone())).await;
        d.step("target-double-slash", fresh_ip(), 400, |_| Req::get("//api/v1/info")).await;
        d.step("target-absolute-form", fresh_ip(), 400, |_| Req::get("https://localhost/api/v1/info")).await;
        d.step("target-asterisk", fresh_ip(), 400, |_| Req::new("OPTIONS", "*")).await;
        d.step("target-query-only", fresh_ip(), 404, |_| Req::get("/?")).await;
    })
}
