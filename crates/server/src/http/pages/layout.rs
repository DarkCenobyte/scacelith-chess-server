//! The shared layout of the few HTML pages the server shows in a browser (the e-mail links). No
//! JavaScript, no external resource, inline styles only, served with the page CSP
//! ([`crate::http::api::PAGE_CSP`]). Colours follow the game: dark wood, ivory, brass.

pub use crate::http::api::escape_html;

const STYLE: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
  padding: 24px 16px; font: 17px/1.55 Georgia, "Times New Roman", serif; color: #efe6d2;
  background: #1b130d radial-gradient(ellipse at top, #3a2819 0%, #1b130d 70%); }
main { width: 100%; max-width: 440px; background: linear-gradient(#2c2017, #241a12); border: 1px solid #5a4330;
  border-radius: 10px; padding: 32px 28px; box-shadow: 0 18px 50px rgba(0,0,0,.55), inset 0 1px 0 rgba(255,240,210,.06); }
.brand { margin: 0 0 18px; font-size: 13px; letter-spacing: .18em; text-transform: uppercase; color: #c9a45c; }
h1 { margin: 0 0 14px; font-weight: normal; font-size: 26px; line-height: 1.25; color: #f6eedc; }
p { margin: 0 0 14px; color: #dcd0b8; }
.note { font-size: 14px; color: #a8987c; }
.error { background: rgba(160,50,40,.18); border: 1px solid #8a3a2e; color: #f0c8bd; border-radius: 6px; padding: 10px 12px; }
.ok { color: #cfe3b5; }
label { display: block; margin: 16px 0 6px; font-size: 15px; color: #dcd0b8; }
input[type=password] { width: 100%; padding: 11px 12px; font: inherit; color: #1f160f; background: #f3ead8;
  border: 1px solid #b89b6a; border-radius: 6px; }
input[type=password]:focus { outline: 2px solid #c9a45c; outline-offset: 1px; }
button { margin-top: 22px; width: 100%; padding: 12px 16px; font: inherit; font-size: 17px; cursor: pointer;
  color: #1f160f; background: linear-gradient(#f6eedc, #e4d6b8); border: 1px solid #b89b6a; border-radius: 6px; }
button:hover { background: linear-gradient(#fff8e8, #eadcbf); }
"#;

/// The tone of a message page's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain text.
    Plain,
    /// A success (`class="ok"`).
    Ok,
    /// A refusal or a failure (`class="error"`).
    Error,
}

/// A complete page; `body` is trusted HTML.
pub fn render_page(server_name: &str, title: &str, body: &str) -> String {
    let (name, title) = (escape_html(server_name), escape_html(title));
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><meta name=\"robots\" content=\"noindex\">\
         <title>{title} - {name}</title><style>{STYLE}</style></head>\
         <body><main><p class=\"brand\">{name}</p>{body}</main></body></html>\n"
    )
}

/// A page with a heading, a message and an optional note (results, errors).
pub fn render_message(server_name: &str, title: &str, message: &str, note: &str, tone: Tone) -> String {
    let class = match tone {
        Tone::Plain => "",
        Tone::Ok => " class=\"ok\"",
        Tone::Error => " class=\"error\"",
    };
    let mut body = format!("<h1>{}</h1><p{class}>{}</p>", escape_html(title), escape_html(message));
    if !note.is_empty() {
        body.push_str(&format!("<p class=\"note\">{}</p>", escape_html(note)));
    }
    render_page(server_name, title, &body)
}

/// The error page of the page routes ("Request refused" or "Server error" and the error's
/// message), for [`crate::http::ApiBuilder::page_renderer`].
pub fn error_page_renderer(server_name: String) -> impl Fn(&str, &str) -> String + Send + Sync + 'static {
    move |title, message| render_message(&server_name, title, message, "", Tone::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_escape_their_text() {
        let page = render_message("Club <b>", "Done & dusted", "It's \"ok\"", "a note", Tone::Ok);
        assert!(page.starts_with("<!DOCTYPE html>\n<html lang=\"en\">"));
        assert!(page.contains("<title>Done &amp; dusted - Club &lt;b&gt;</title>"));
        assert!(page.contains("<p class=\"brand\">Club &lt;b&gt;</p>"));
        assert!(page.contains("<h1>Done &amp; dusted</h1><p class=\"ok\">It&#39;s &quot;ok&quot;</p><p class=\"note\">a note</p>"));
        assert!(page.ends_with("</main></body></html>\n"));
        let plain = render_message("S", "T", "M", "", Tone::Plain);
        assert!(plain.contains("<h1>T</h1><p>M</p></main>"));
        let error = error_page_renderer("S".into())("Request refused", "Too many requests; try again later.");
        assert!(error.contains(
            "<h1>Request refused</h1><p class=\"error\">Too many requests; try again later.</p></main>"
        ));
    }
}
