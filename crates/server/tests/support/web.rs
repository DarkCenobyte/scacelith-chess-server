//! Plain HTTP requests of the black-box tests: the HTML pages of the e-mail links (a GET, or a
//! form posted as a browser does), the links of the mails, and a small GIF reader.

use scacelith_client::http::{HttpConnection, Request, Response};

use super::server::TestServer;

/// `application/x-www-form-urlencoded`.
pub const FORM: &str = "application/x-www-form-urlencoded";

/// Percent-encodes a form value or a query parameter.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes a percent-encoded query value (`+` is a space).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A request to `path` (outside `/api/v1` unless it starts with it), with an optional form body,
/// on a new connection: the answer whatever its status.
pub async fn page(srv: &TestServer, method: &str, path: &str, form: Option<&[(&str, &str)]>) -> Response {
    let body = form
        .map(|pairs| {
            pairs.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&")
        })
        .unwrap_or_default();
    let req = Request {
        method,
        target: path,
        bearer: None,
        content_type: form.map(|_| FORM),
        body: body.as_bytes(),
    };
    let mut conn = HttpConnection::open(&srv.endpoint()).await.expect("connected");
    conn.send(&req).await.expect("an answer")
}

/// The path and query of the first `https://` link of a mail's text.
pub fn link_path(text: &str) -> String {
    let start = text.find("https://").unwrap_or_else(|| panic!("no link in {text:?}"));
    let link = text[start..].split_whitespace().next().expect("a link");
    let after_scheme = &link["https://".len()..];
    let slash = after_scheme.find('/').unwrap_or(after_scheme.len());
    after_scheme[slash..].to_string()
}

/// The value of the query parameter `name` of a path (decoded).
pub fn query_param(path: &str, name: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(k) == name).then(|| decode(v))
    })
}

/// The answer's body as text.
pub fn text(res: &Response) -> String {
    String::from_utf8_lossy(&res.body).into_owned()
}

/// What a test reads of a GIF: the screen size, the frames' delays (hundredths of a second) and
/// whether it loops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GifInfo {
    pub width: u16,
    pub height: u16,
    pub delays: Vec<u16>,
    pub loops: bool,
}

/// Walks the blocks of a GIF89a file (the image data is skipped, not decompressed).
pub fn read_gif(b: &[u8]) -> GifInfo {
    assert!(b.len() >= 13 && &b[..6] == b"GIF89a", "not a GIF89a file");
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let table = |packed: u8| if packed & 0x80 != 0 { 3usize << ((packed & 7) + 1) } else { 0 };
    let skip_blocks = |mut i: usize| -> usize {
        loop {
            let n = usize::from(b[i]);
            i += 1;
            if n == 0 {
                return i;
            }
            i += n;
        }
    };
    let mut out = GifInfo { width: u16_at(6), height: u16_at(8), delays: Vec::new(), loops: false };
    let mut i = 13 + table(b[10]);
    let mut delay = 0;
    loop {
        match b[i] {
            0x21 if b[i + 1] == 0xF9 => {
                delay = u16_at(i + 4);
                i = skip_blocks(i + 2);
            }
            0x21 if b[i + 1] == 0xFF => {
                out.loops |= b.get(i + 3..i + 14) == Some(&b"NETSCAPE2.0"[..]);
                i = skip_blocks(i + 2);
            }
            0x21 => i = skip_blocks(i + 2),
            0x2C => {
                out.delays.push(delay);
                i += 10 + table(b[i + 9]);
                i = skip_blocks(i + 1);
            }
            0x3B => return out,
            other => panic!("unexpected GIF block {other:#x} at byte {i}"),
        }
    }
}
