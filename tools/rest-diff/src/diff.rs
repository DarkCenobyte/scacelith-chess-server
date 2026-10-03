//! Compares two normalised answers and lists every difference: status line, transport, the
//! headers (all of them except `Date`, names compared without case, values exactly or within
//! the tolerance of `Retry-After`), connection close, and the body (JSON with key order and
//! number text, normalised text line by line, binary by hash).

use crate::json::J;
use crate::normalize::{Body, Normalized};

/// One difference between the two answers.
#[derive(Clone, Debug)]
pub struct Difference {
    /// What differs: `status`, `header content-type`, `body $.user.id`...
    pub aspect: String,
    /// The Node server's side.
    pub node: String,
    /// The Rust server's side.
    pub rust: String,
}

impl Difference {
    fn new(aspect: impl Into<String>, node: impl Into<String>, rust: impl Into<String>) -> Difference {
        Difference { aspect: aspect.into(), node: node.into(), rust: rust.into() }
    }
}

/// Every difference between the answers of the two servers.
pub fn compare(node: &Normalized, rust: &Normalized) -> Vec<Difference> {
    let mut out = Vec::new();
    if node.error != rust.error {
        out.push(Difference::new(
            "transport",
            node.error.clone().unwrap_or_else(|| "answer".into()),
            rust.error.clone().unwrap_or_else(|| "answer".into()),
        ));
    }
    if node.status != rust.status {
        out.push(Difference::new("status", node.status.to_string(), rust.status.to_string()));
    } else if node.reason != rust.reason {
        out.push(Difference::new("reason phrase", &node.reason, &rust.reason));
    }
    if node.version != rust.version {
        out.push(Difference::new("protocol", &node.version, &rust.version));
    }
    compare_headers(node, rust, &mut out);
    if let (Some(a), Some(b)) = (node.closed, rust.closed)
        && a != b
    {
        let s = |c: bool| if c { "closed after the answer" } else { "kept open" };
        out.push(Difference::new("connection", s(a), s(b)));
    }
    if node.chunked != rust.chunked {
        out.push(Difference::new("transfer coding", chunk_name(node.chunked), chunk_name(rust.chunked)));
    }
    compare_bodies(node, rust, &mut out);
    out
}

fn chunk_name(c: bool) -> &'static str {
    if c { "chunked" } else { "length" }
}

fn compare_headers(node: &Normalized, rust: &Normalized, out: &mut Vec<Difference>) {
    let mut names: Vec<String> = Vec::new();
    for (k, _) in node.headers.iter().chain(rust.headers.iter()) {
        let l = k.to_ascii_lowercase();
        if !names.contains(&l) {
            names.push(l);
        }
    }
    let same_raw_body = node.raw_sha == rust.raw_sha;
    for name in names {
        let pick = |n: &Normalized| -> Vec<(String, String)> {
            n.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&name)).cloned().collect()
        };
        let (a, b) = (pick(node), pick(rust));
        let show = |v: &[(String, String)]| {
            if v.is_empty() {
                "(absent)".to_string()
            } else {
                v.iter().map(|(_, x)| x.as_str()).collect::<Vec<_>>().join(" | ")
            }
        };
        let aspect = format!("header {name}");
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            out.push(Difference::new(aspect, show(&a), show(&b)));
            continue;
        }
        let names_a: Vec<&str> = a.iter().map(|(k, _)| k.as_str()).collect();
        let names_b: Vec<&str> = b.iter().map(|(k, _)| k.as_str()).collect();
        if names_a != names_b {
            out.push(Difference::new(format!("header name case {name}"), names_a.join(","), names_b.join(",")));
        }
        let equal = match name.as_str() {
            // The length follows the body: compared when the raw bodies are identical, the body
            // comparison covers the rest.
            "content-length" => !same_raw_body || a == b,
            "retry-after" => a.iter().zip(&b).all(|((_, x), (_, y))| match (x.parse::<i64>(), y.parse::<i64>()) {
                (Ok(p), Ok(q)) => (p - q).abs() <= 1,
                _ => x == y,
            }),
            _ => a.iter().zip(&b).all(|((_, x), (_, y))| x == y),
        };
        if !equal {
            out.push(Difference::new(aspect, show(&a), show(&b)));
        }
    }
}

fn compare_bodies(node: &Normalized, rust: &Normalized, out: &mut Vec<Difference>) {
    match (&node.body, &rust.body) {
        (Body::Empty, Body::Empty) => {}
        (Body::Json { value: a, compact: ca }, Body::Json { value: b, compact: cb }) => {
            if ca != cb {
                out.push(Difference::new("body whitespace", compact_name(*ca), compact_name(*cb)));
            }
            let before = out.len();
            compare_json("$", a, b, out);
            if out.len() - before > 25 {
                out.truncate(before + 25);
                out.push(Difference::new("body", "(more differences)", "(more differences)"));
            }
        }
        (Body::Text(a), Body::Text(b)) => {
            if a != b {
                let (line, la, lb) = first_line_difference(a, b);
                out.push(Difference::new(format!("body text line {line}"), la, lb));
            }
        }
        (Body::Binary, Body::Binary) => {
            if node.raw_sha != rust.raw_sha {
                out.push(Difference::new(
                    "body bytes",
                    format!("{} bytes sha256 {}", node.raw_len, &node.raw_sha[..16]),
                    format!("{} bytes sha256 {}", rust.raw_len, &rust.raw_sha[..16]),
                ));
            }
        }
        (a, b) => out.push(Difference::new("body kind", describe(a, node), describe(b, rust))),
    }
}

fn compact_name(c: bool) -> &'static str {
    if c { "compact" } else { "with whitespace" }
}

fn describe(b: &Body, n: &Normalized) -> String {
    match b {
        Body::Empty => "empty".into(),
        Body::Json { value, .. } => format!("JSON {}", clip(&value.to_text())),
        Body::Text(t) => format!("text {}", clip(t)),
        Body::Binary => format!("{} binary bytes", n.raw_len),
    }
}

/// Shortens a value for the report.
pub fn clip(s: &str) -> String {
    let flat = s.replace('\n', "\\n");
    if flat.chars().count() > 300 {
        let mut t: String = flat.chars().take(300).collect();
        t.push_str("...");
        t
    } else {
        flat
    }
}

fn first_line_difference(a: &str, b: &str) -> (usize, String, String) {
    let la: Vec<&str> = a.split('\n').collect();
    let lb: Vec<&str> = b.split('\n').collect();
    for i in 0..la.len().max(lb.len()) {
        let x = la.get(i).copied();
        let y = lb.get(i).copied();
        if x != y {
            return (i + 1, clip(x.unwrap_or("(end of text)")), clip(y.unwrap_or("(end of text)")));
        }
    }
    (0, String::new(), String::new())
}

fn kind_name(v: &J) -> &'static str {
    match v {
        J::Null => "null",
        J::Bool(_) => "boolean",
        J::Num(_) => "number",
        J::Str(..) => "string",
        J::Arr(_) => "array",
        J::Obj(_) => "object",
        J::Mask(_) => "masked",
        J::Approx { .. } => "number~",
    }
}

fn compare_json(path: &str, a: &J, b: &J, out: &mut Vec<Difference>) {
    match (a, b) {
        (J::Obj(ma), J::Obj(mb)) => {
            let ka: Vec<&str> = ma.iter().map(|(k, _)| k.as_str()).collect();
            let kb: Vec<&str> = mb.iter().map(|(k, _)| k.as_str()).collect();
            if ka != kb {
                let missing: Vec<&str> = ka.iter().filter(|k| !kb.contains(k)).copied().collect();
                let extra: Vec<&str> = kb.iter().filter(|k| !ka.contains(k)).copied().collect();
                if missing.is_empty() && extra.is_empty() {
                    out.push(Difference::new(format!("body {path} key order"), ka.join(","), kb.join(",")));
                } else {
                    out.push(Difference::new(
                        format!("body {path} keys"),
                        format!("only node: [{}]", missing.join(",")),
                        format!("only rust: [{}]", extra.join(",")),
                    ));
                }
            }
            for (k, va) in ma {
                if let Some((_, vb)) = mb.iter().find(|(kb, _)| kb == k) {
                    compare_json(&format!("{path}.{k}"), va, vb, out);
                }
            }
        }
        (J::Arr(xa), J::Arr(xb)) => {
            if xa.len() != xb.len() {
                out.push(Difference::new(
                    format!("body {path} length"),
                    format!("{} items: {}", xa.len(), clip(&a.to_text())),
                    format!("{} items: {}", xb.len(), clip(&b.to_text())),
                ));
            }
            for (i, (va, vb)) in xa.iter().zip(xb).enumerate() {
                compare_json(&format!("{path}[{i}]"), va, vb, out);
            }
        }
        (J::Str(sa, ra), J::Str(sb, rb)) => {
            if sa != sb {
                out.push(Difference::new(format!("body {path}"), clip(&a.to_text()), clip(&b.to_text())));
            } else if ra != rb {
                out.push(Difference::new(format!("body {path} escaping"), clip(ra), clip(rb)));
            }
        }
        (J::Approx { value: va, tol: ta, .. }, J::Approx { value: vb, tol: tb, .. }) => {
            if (va - vb).abs() > ta.max(*tb) {
                out.push(Difference::new(format!("body {path}"), va.to_string(), vb.to_string()));
            }
        }
        _ => {
            if a != b {
                let detail = |v: &J| format!("{} {}", kind_name(v), clip(&v.to_text()));
                out.push(Difference::new(format!("body {path}"), detail(a), detail(b)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Resp;
    use crate::normalize::{Context, normalize};

    fn resp(status: u16, body: &str) -> Resp {
        Resp {
            status,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Content-Type".into(), "application/json; charset=utf-8".into()),
                ("Date".into(), "x".into()),
            ],
            body: body.as_bytes().to_vec(),
            ..Resp::default()
        }
    }

    #[test]
    fn finds_key_order_numbers_and_status() {
        let ctx = Context::default();
        let a = normalize(&resp(200, r#"{"a":1,"b":2.0,"t":1790000000000}"#), &ctx);
        let b = normalize(&resp(200, r#"{"b":2,"a":1,"t":1790000005000}"#), &ctx);
        let d = compare(&a, &b);
        let aspects: Vec<&str> = d.iter().map(|x| x.aspect.as_str()).collect();
        assert_eq!(aspects, vec!["body $ key order", "body $.b"]);
        let c = normalize(&resp(201, r#"{"a":1,"b":2.0,"t":1790000000000}"#), &ctx);
        assert_eq!(compare(&a, &c)[0].aspect, "status");
        assert!(compare(&a, &a).is_empty());
    }
}
