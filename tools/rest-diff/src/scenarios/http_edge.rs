//! The HTTP layer: request bodies (content types, charsets, JSON errors, strict schemas, the
//! size limit, the body deadline), malformed requests (framing, methods, headers, versions),
//! oversized headers and the forms of the `Authorization` header.

use serde_json::json;

use super::BoxFut;
use crate::duo::{Duo, fresh_ip};
use crate::http::Req;

const LOGIN: &str = "/api/v1/auth/login";

/// The text with each `BS` replaced by a backslash (JSON escapes written without escaping).
fn backslashes(text: &str) -> Vec<u8> {
    text.replace("BS", "\\").into_bytes()
}

fn raw(text: &str) -> Req {
    Req::raw(text.as_bytes().to_vec())
}

/// The scenario.
pub fn run(d: &mut Duo) -> BoxFut<'_> {
    Box::pin(async move {
        bodies(d).await;
        schemas(d).await;
        framing(d).await;
        authorization(d).await;
    })
}

/// Content types, charsets, JSON syntax, the body limit.
async fn bodies(d: &mut Duo) {
    // The login endpoint takes a body and checks it before any password work; a fresh address
    // per few steps keeps the `auth` limit (20 per 10 minutes) out of the way.
    let mut ip = fresh_ip();
    let mut n = 0;
    let mut next = |n: &mut u32| {
        *n += 1;
        if n.is_multiple_of(15) {
            ip = fresh_ip();
        }
        ip
    };
    // A login name per case: five failures on one name start its failure delay.
    let counter = std::cell::Cell::new(0u32);
    let body = || {
        counter.set(counter.get() + 1);
        format!(r#"{{"login":"nobody{}","password":"x"}}"#, counter.get()).into_bytes()
    };
    let cases: Vec<(&str, Req, u16)> = vec![
        ("text-plain", Req::post(LOGIN).body_bytes("text/plain", body()), 415),
        ("no-content-type", Req::post(LOGIN).body_untyped(body()), 415),
        ("latin1", Req::post(LOGIN).body_bytes("application/json; charset=latin1", body()), 415),
        ("charset-upper", Req::post(LOGIN).body_bytes("application/json; charset=UTF-8", body()), 401),
        ("charset-quoted", Req::post(LOGIN).body_bytes("application/json; charset=\"utf-8\"", body()), 401),
        ("charset-utf8", Req::post(LOGIN).body_bytes("application/json;charset=utf8", body()), 401),
        ("charset-empty", Req::post(LOGIN).body_bytes("application/json; charset=", body()), 0),
        ("type-upper", Req::post(LOGIN).body_bytes("Application/JSON", body()), 401),
        ("type-params", Req::post(LOGIN).body_bytes("application/json; foo=bar", body()), 401),
        ("type-json-suffix", Req::post(LOGIN).body_bytes("application/problem+json", body()), 415),
        (
            "type-form",
            Req::post(LOGIN).body_bytes("application/x-www-form-urlencoded", b"login=a&password=b".to_vec()),
            415,
        ),
        ("type-spaces", Req::post(LOGIN).body_bytes(" application/json ", body()), 401),
        ("type-multipart", Req::post(LOGIN).body_bytes("multipart/form-data; boundary=x", body()), 415),
        ("empty-body-typed", Req::post(LOGIN).body_bytes("application/json", Vec::new()), 400),
        ("empty-body-untyped", Req::post(LOGIN), 400),
        ("empty-body-text-plain", Req::post(LOGIN).body_bytes("text/plain", Vec::new()), 400),
        ("invalid-json", Req::post(LOGIN).body_bytes("application/json", b"{login".to_vec()), 400),
        ("json-trailing", Req::post(LOGIN).body_bytes("application/json", b"{} x".to_vec()), 400),
        ("json-array", Req::post(LOGIN).body_bytes("application/json", b"[]".to_vec()), 400),
        ("json-null", Req::post(LOGIN).body_bytes("application/json", b"null".to_vec()), 400),
        ("json-string", Req::post(LOGIN).body_bytes("application/json", b"\"x\"".to_vec()), 400),
        ("json-number", Req::post(LOGIN).body_bytes("application/json", b"12".to_vec()), 400),
        ("json-whitespace", Req::post(LOGIN).body_bytes("application/json", b"  ".to_vec()), 400),
        ("json-bom", Req::post(LOGIN).body_bytes("application/json", b"\xef\xbb\xbf{}".to_vec()), 400),
        (
            "json-invalid-utf8",
            Req::post(LOGIN).body_bytes("application/json", b"{\"login\":\"\xff\"}".to_vec()),
            400,
        ),
        (
            "json-duplicate-key",
            Req::post(LOGIN)
                .body_bytes("application/json", br#"{"login":"a","login":"nobody","password":"x"}"#.to_vec()),
            401,
        ),
        (
            "json-deep",
            Req::post(LOGIN).body_bytes(
                "application/json",
                format!("{}{}", "[".repeat(200), "]".repeat(200)).into_bytes(),
            ),
            400,
        ),
        (
            "json-deep-object",
            Req::post(LOGIN).body_bytes(
                "application/json",
                format!("{{\"a\":{}1{}}}", "[".repeat(150), "]".repeat(150)).into_bytes(),
            ),
            400,
        ),
        (
            "json-escape-unicode",
            Req::post(LOGIN)
                .body_bytes("application/json", backslashes(r#"{"login":"BSu006eobody","password":"x"}"#)),
            401,
        ),
        (
            "json-nul-char",
            Req::post(LOGIN)
                .body_bytes("application/json", backslashes(r#"{"login":"aBSu0000","password":"x"}"#)),
            400,
        ),
    ];
    for (name, req, expect) in cases {
        let at = next(&mut n);
        d.step(&format!("body-{name}"), at, expect, |_| req.clone()).await;
    }

    // The body limit (16384 bytes): at the limit, one byte over, a declared length over it.
    let fill = |len: usize| {
        let prefix = br#"{"login":"nobody","password":"x","pad":""#;
        let mut v = prefix.to_vec();
        v.resize(len - 2, b'a');
        v.extend_from_slice(b"\"}");
        v
    };
    d.step("body-at-limit", fresh_ip(), 400, |_| {
        Req::post(LOGIN).body_bytes("application/json", fill(16384))
    })
    .await;
    d.step("body-over-limit", fresh_ip(), 413, |_| {
        Req::post(LOGIN).body_bytes("application/json", fill(16385)).fresh()
    })
    .await;
    d.step("body-declared-over-limit", fresh_ip(), 413, |_| {
        raw("POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 999999\r\n\r\n{")
    })
    .await;
    d.step("body-limit-on-get", fresh_ip(), 0, |_| {
        Req::get("/api/v1/info").body_bytes("application/json", vec![b' '; 20000]).fresh()
    })
    .await;
    d.step("body-on-get-small", fresh_ip(), 200, |_| {
        Req::get("/api/v1/info").body_bytes("application/json", b"{}".to_vec())
    })
    .await;
    d.step("body-on-unknown-path", fresh_ip(), 404, |_| {
        Req::post("/api/v1/nope").body_bytes("application/json", fill(20000)).fresh()
    })
    .await;
    d.step("body-on-405", fresh_ip(), 405, |_| {
        Req::post("/api/v1/info").body_bytes("application/json", fill(20000)).fresh()
    })
    .await;
    // Chunked bodies.
    d.step("body-chunked", fresh_ip(), 401, |_| {
        raw("POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nf\r\n{\"login\":\"nobod\r\n12\r\ny\",\"password\":\"x\"}\r\n0\r\n\r\n")
    })
    .await;
    d.step("body-chunked-over-limit", fresh_ip(), 413, |_| {
        let mut s = String::from("POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n");
        for _ in 0..5 {
            s.push_str(&format!("1000\r\n{}\r\n", "a".repeat(4096)));
        }
        s.push_str("0\r\n\r\n");
        raw(&s)
    })
    .await;
    // The body deadline: a declared body that never arrives (10 s).
    d.step("body-timeout", fresh_ip(), 408, |_| {
        raw("POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n{\"login\":")
    })
    .await;
}

/// Strict schemas: unknown fields, types, lengths, control characters.
async fn schemas(d: &mut Duo) {
    let ip = fresh_ip();
    let cases = vec![
        ("unknown-field", json!({"login": "a", "password": "b", "extra": 1}), 400),
        ("missing-password", json!({"login": "a"}), 400),
        ("missing-login", json!({"password": "a"}), 400),
        ("login-number", json!({"login": 12, "password": "a"}), 400),
        ("login-null", json!({"login": null, "password": "a"}), 400),
        ("login-empty", json!({"login": "", "password": "a"}), 400),
        ("login-too-long", json!({"login": "a".repeat(255), "password": "a"}), 400),
        ("login-control", json!({"login": "a\tb", "password": "a"}), 400),
        ("login-del", json!({"login": "a\u{7f}b", "password": "a"}), 400),
        ("password-too-long", json!({"login": "a", "password": "p".repeat(1025)}), 400),
        ("password-array", json!({"login": "a", "password": ["x"]}), 400),
        ("client-label-too-long", json!({"login": "a", "password": "b", "clientLabel": "c".repeat(65)}), 400),
        ("client-label-null", json!({"login": "a", "password": "b", "clientLabel": null}), 400),
        ("pow-not-object", json!({"login": "a", "password": "b", "pow": "x"}), 400),
        (
            "pow-unknown-field",
            json!({"login": "a", "password": "b", "pow": {"challenge": "c", "nonce": "1", "x": 1}}),
            400,
        ),
        ("pow-missing-nonce", json!({"login": "a", "password": "b", "pow": {"challenge": "c"}}), 400),
        ("unicode-length", json!({"login": "é".repeat(254), "password": "x"}), 401),
        ("astral-length", json!({"login": "\u{1F600}".repeat(127), "password": "x"}), 401),
        ("astral-too-long", json!({"login": "\u{1F600}".repeat(128), "password": "x"}), 400),
        ("first-error-order", json!({"zzz": 1, "login": 5}), 400),
    ];
    for (i, (name, body, expect)) in cases.into_iter().enumerate() {
        let at = if i < 15 { ip } else { fresh_ip() };
        d.step(&format!("schema-{name}"), at, expect, |_| Req::post(LOGIN).json(body.clone())).await;
    }
    // Other endpoints' schemas.
    let ip = fresh_ip();
    let others = vec![
        ("register-empty", "/api/v1/auth/register", json!({}), 400),
        (
            "register-username-long",
            "/api/v1/auth/register",
            json!({"username": "u".repeat(65), "email": "a@b.c", "password": "x"}),
            400,
        ),
        (
            "register-email-long",
            "/api/v1/auth/register",
            json!({"username": "u", "email": format!("{}@b.cd", "e".repeat(251)), "password": "x"}),
            400,
        ),
        ("resend-number", "/api/v1/auth/verify-email/resend", json!({"email": 5}), 400),
        ("forgot-extra", "/api/v1/auth/password/forgot", json!({"email": "a@b.cd", "x": true}), 400),
        (
            "reset-token-long",
            "/api/v1/auth/password/reset",
            json!({"token": "t".repeat(129), "newPassword": "x"}),
            400,
        ),
        ("mfa-token-missing", "/api/v1/auth/login/mfa", json!({"code": "123456"}), 400),
        ("mfa-no-code", "/api/v1/auth/login/mfa", json!({"mfaToken": "mfa_x"}), 401),
        (
            "mfa-code-long",
            "/api/v1/auth/login/mfa",
            json!({"mfaToken": "mfa_x", "code": "1".repeat(33)}),
            400,
        ),
        (
            "sso-start-schema",
            "/api/v1/auth/sso/google/start",
            json!({"codeChallenge": "x", "redirectPort": 80}),
            0,
        ),
        ("sso-finish-schema", "/api/v1/auth/sso/google/finish", json!({}), 0),
        (
            "sso-link-schema",
            "/api/v1/auth/sso/google/link",
            json!({"linkTicket": "sso_x", "password": "p"}),
            0,
        ),
        (
            "sso-complete-schema",
            "/api/v1/auth/sso/complete",
            json!({"ssoTicket": "sso_x", "username": "abc"}),
            0,
        ),
    ];
    for (name, path, body, expect) in others {
        d.step(&format!("schema-{name}"), ip, expect, |_| Req::post(path).json(body.clone())).await;
    }
}

/// Malformed requests and the HTTP framing.
async fn framing(d: &mut Duo) {
    let cases: Vec<(&str, String, u16)> = vec![
        ("http10", "GET /api/v1/info HTTP/1.0\r\n\r\n".into(), 200),
        ("http10-keepalive", "GET /api/v1/info HTTP/1.0\r\nConnection: keep-alive\r\n\r\n".into(), 200),
        ("connection-close", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n".into(), 200),
        ("no-host", "GET /api/v1/info HTTP/1.1\r\n\r\n".into(), 0),
        ("two-hosts", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nHost: other\r\n\r\n".into(), 0),
        ("unknown-method", "FOO /api/v1/info HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("lower-case-method", "get /api/v1/info HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("connect-method", "CONNECT localhost:443 HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("trace-method", "TRACE /api/v1/info HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("http2-preface", "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".into(), 0),
        ("http11-bad-version", "GET /api/v1/info HTTP/1.2\r\nHost: {host}\r\n\r\n".into(), 0),
        ("http09", "GET /api/v1/info\r\n\r\n".into(), 0),
        ("garbage", "HELLO\r\n\r\n".into(), 0),
        ("space-in-target", "GET /api/v1/in fo HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("header-no-colon", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nBroken\r\n\r\n".into(), 0),
        ("header-space-before-colon", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nX-A : 1\r\n\r\n".into(), 0),
        ("header-obs-fold", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nX-A: 1\r\n 2\r\n\r\n".into(), 0),
        ("bare-lf", "GET /api/v1/info HTTP/1.1\nHost: {host}\n\n".into(), 0),
        ("cl-invalid", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: abc\r\n\r\n{}".into(), 0),
        ("cl-negative", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: -1\r\n\r\n{}".into(), 0),
        ("cl-double-same", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".into(), 0),
        ("cl-double-different", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n{} ".into(), 0),
        ("cl-plus", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: +2\r\n\r\n{}".into(), 0),
        ("cl-huge", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 99999999999999999999\r\n\r\n{}".into(), 0),
        ("te-unknown", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: gzip\r\n\r\n{}".into(), 0),
        ("te-chunked-bad-size", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n".into(), 0),
        ("expect-continue", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n{}".into(), 400),
        ("expect-unknown", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nExpect: something\r\nContent-Length: 2\r\n\r\n{}".into(), 0),
        ("upgrade-h2c", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAARAAAAAAAIAAAAA\r\n\r\n".into(), 0),
        ("upgrade-websocket-api", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".into(), 0),
        ("pipelined", "GET /api/v1/healthz HTTP/1.1\r\nHost: {host}\r\n\r\nGET /api/v1/readyz HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 200),
        ("fragment", "GET /api/v1/info#frag HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("raw-quote-in-query", "GET /api/v1/info?a=\"b\" HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("raw-utf8-in-path", "GET /api/v1/players/\u{e9}t\u{e9} HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("raw-utf8-in-query", "GET /api/v1/info?q=\u{e9} HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("raw-utf8-in-header-value", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nX-A: \u{e9}\r\n\r\n".into(), 0),
        ("raw-del-in-path", "GET /api/v1/info\x7f HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
        ("cl-list-same", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2, 2\r\n\r\n{}".into(), 0),
        ("cl-and-te", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n".into(), 0),
        ("cl-double-after-empty-lines", "\r\n\r\nPOST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".into(), 0),
        ("te-chunked-bad-ending", "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}XX0\r\n\r\n".into(), 0),
        ("nul-in-header", "GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\nX-A: a\0b\r\n\r\n".into(), 0),
        ("ctl-in-target", "GET /api/v1/info?\x01 HTTP/1.1\r\nHost: {host}\r\n\r\n".into(), 0),
    ];
    for (name, text, expect) in cases {
        d.step(&format!("raw-{name}"), fresh_ip(), expect, |_| raw(&text)).await;
    }
    // Header sizes: Node's limit is 8192 bytes counted as URL + header names + values (the
    // ephemeral ports have 5 digits: "localhost:NNNNN" is 15 characters).
    let big =
        |n: usize| format!("GET /api/v1/info HTTP/1.1\r\nHost: {{host}}\r\nX-Big: {}\r\n\r\n", "b".repeat(n));
    let at_limit = 8192 - "/api/v1/info".len() - "Host".len() - 15 - "X-Big".len();
    d.step("header-head-8192", fresh_ip(), 0, |_| raw(&big(at_limit))).await;
    d.step("header-head-8193", fresh_ip(), 0, |_| raw(&big(at_limit + 1))).await;
    d.step("header-15k", fresh_ip(), 0, |_| raw(&big(15_000))).await;
    d.step("header-17k", fresh_ip(), 0, |_| raw(&big(17_000))).await;
    d.step("header-64k", fresh_ip(), 0, |_| raw(&big(64_000))).await;
    d.step("header-200k", fresh_ip(), 0, |_| raw(&big(200_000))).await;
    // A head over hyper's read buffer (9216 bytes) whose URL, names and values stay small:
    // llhttp does not count the whitespace before a value.
    let padded = |headers: &str| {
        format!(
            "POST {LOGIN} HTTP/1.1\r\nHost: {{host}}\r\nContent-Type: application/json\r\nX-Pad:{}v\r\n{headers}\r\n{{}}",
            " ".repeat(9300)
        )
    };
    d.step("header-padded-9300", fresh_ip(), 0, |_| raw(&padded("Content-Length: 2\r\n"))).await;
    d.step("header-padded-9300-cl-and-te", fresh_ip(), 0, |_| {
        raw(&padded("Content-Length: 2\r\nTransfer-Encoding: chunked\r\n"))
    })
    .await;
    // Chunk extensions and trailers: llhttp allows 16 KiB of extensions per chunk and trailers
    // within the limits of a head; hyper 16 KiB of extensions per body and 16 KiB, 100 lines of
    // trailers.
    let chunked = |body: String| {
        format!(
            "POST {LOGIN} HTTP/1.1\r\nHost: {{host}}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{body}"
        )
    };
    let ext = "a".repeat(6000);
    let trailer_lines = |n: usize| (0..n).map(|i| format!("X-T{i}: v\r\n")).collect::<String>();
    let chunk_cases = [
        ("chunk-extensions-17k", format!("2;{}\r\n{{}}\r\n0\r\n\r\n", "a".repeat(17_000))),
        ("chunk-extensions-3x6000", format!("1;{ext}\r\n{{\r\n1;{ext}\r\n}}\r\n1;{ext}\r\n \r\n0\r\n\r\n")),
        ("chunk-trailers-9000", format!("2\r\n{{}}\r\n0\r\nX-T: {}\r\n\r\n", "a".repeat(9000))),
        ("chunk-trailers-17k", format!("2\r\n{{}}\r\n0\r\nX-T: {}\r\n\r\n", "a".repeat(17_000))),
        ("chunk-trailers-70-lines", format!("2\r\n{{}}\r\n0\r\n{}\r\n", trailer_lines(70))),
        ("chunk-trailers-101-lines", format!("2\r\n{{}}\r\n0\r\n{}\r\n", trailer_lines(101))),
    ];
    for (name, body) in chunk_cases {
        let text = chunked(body);
        d.step(name, fresh_ip(), 0, |_| raw(&text)).await;
    }
    let many = |n: usize| {
        let mut s = String::from("GET /api/v1/info HTTP/1.1\r\nHost: {host}\r\n");
        for i in 0..n {
            s.push_str(&format!("X-H{i}: v\r\n"));
        }
        s.push_str("\r\n");
        s
    };
    // Header lines (Host included): `headers-N` sends N + 1.
    for n in [50, 63, 64, 65, 98, 99, 100] {
        d.step(&format!("headers-{n}"), fresh_ip(), 0, |_| raw(&many(n))).await;
    }
    d.step("headers-1000", fresh_ip(), 0, |_| raw(&many(1000))).await;
    d.step("headers-3000", fresh_ip(), 0, |_| raw(&many(3000))).await;
    let same_name = |n: usize| {
        let lines: String = (0..n).map(|i| format!("Cookie: c{i}=1\r\n")).collect();
        format!("GET /api/v1/info HTTP/1.1\r\nHost: {{host}}\r\n{lines}\r\n")
    };
    d.step("headers-63-same-name", fresh_ip(), 0, |_| raw(&same_name(63))).await;
    d.step("headers-64-same-name", fresh_ip(), 0, |_| raw(&same_name(64))).await;
    let upgrade = |n: usize| {
        let lines: String = (0..n).map(|i| format!("X-H{i}: v\r\n")).collect();
        format!(
            "GET /ws HTTP/1.1\r\nHost: {{host}}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{lines}\r\n"
        )
    };
    d.step("headers-64-upgrade", fresh_ip(), 0, |_| raw(&upgrade(59))).await;
    d.step("headers-65-upgrade", fresh_ip(), 0, |_| raw(&upgrade(60))).await;
}

/// Forms of the `Authorization` header on a session endpoint and an optional-session one.
async fn authorization(d: &mut Duo) {
    let ip = fresh_ip();
    let token = format!("sct_{}", "A".repeat(43));
    let cases: Vec<(&str, Option<String>, u16)> = vec![
        ("none", None, 401),
        ("empty", Some(String::new()), 0),
        ("bearer-only", Some("Bearer".into()), 401),
        ("bearer-space", Some("Bearer ".into()), 401),
        ("unknown-token", Some(format!("Bearer {token}")), 401),
        ("lower-scheme", Some(format!("bearer {token}")), 401),
        ("two-spaces", Some(format!("Bearer  {token}")), 401),
        ("basic", Some("Basic YTpi".into()), 401),
        ("not-sct", Some("Bearer abc".into()), 401),
        ("token-513", Some(format!("Bearer {}", "x".repeat(513))), 401),
        ("token-with-space", Some("Bearer a b".into()), 401),
        ("token-utf8", Some("Bearer \u{e9}t\u{e9}".into()), 0),
    ];
    for (name, header, expect) in cases {
        let h = header.clone();
        d.step(&format!("auth-required-{name}"), ip, expect, move |_| {
            let r = Req::get("/api/v1/account/me");
            match &h {
                Some(v) => r.header("Authorization", v),
                None => r,
            }
        })
        .await;
        let h = header.clone();
        d.step(&format!("auth-optional-{name}"), ip, 0, move |_| {
            let r = Req::get("/api/v1/games/1");
            match &h {
                Some(v) => r.header("Authorization", v),
                None => r,
            }
        })
        .await;
    }
    d.step("auth-on-none-route", ip, 200, |_| Req::get("/api/v1/info").header("Authorization", "Bearer x"))
        .await;
    d.step("auth-two-headers", ip, 0, |_| {
        Req::get("/api/v1/account/me").header("Authorization", "Bearer a").header("Authorization", "Bearer b")
    })
    .await;
    d.step("auth-before-body", ip, 401, |_| {
        Req::post("/api/v1/account/password").body_bytes("text/plain", b"x".to_vec())
    })
    .await;
    d.step("auth-before-404-param", ip, 401, |_| Req::new("DELETE", "/api/v1/auth/sessions/abc")).await;
}
