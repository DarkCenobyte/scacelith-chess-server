//! The e-mail change confirmation page: `GET /confirm-email-change?token=` shows the new address
//! and a button (link scanners must not use the token), `POST /confirm-email-change` applies the
//! change.

use super::layout::{Tone, escape_html, render_message, render_page};

/// The confirmation button, with the new address and the account.
pub fn email_change_form(server_name: &str, token: &str, email: &str, username: &str) -> String {
    render_page(
        server_name,
        "Confirm your new e-mail address",
        &format!(
            "<h1>Confirm your new e-mail address</h1>\
             <p>Press the button to use <strong>{}</strong> for the account <strong>{}</strong>.</p>\
             <p class=\"note\">Messages about the account, password resets included, will then go to this address.</p>\
             <form method=\"post\" action=\"/confirm-email-change\">\
             <input type=\"hidden\" name=\"token\" value=\"{}\">\
             <button type=\"submit\">Use this e-mail address</button></form>",
            escape_html(email),
            escape_html(username),
            escape_html(token)
        ),
    )
}

/// The address changed.
pub fn email_change_done(server_name: &str, email: &str) -> String {
    render_message(
        server_name,
        "E-mail address changed",
        &format!("Your account now uses {email}."),
        "Your devices stay signed in. You can go back to Scacelith.",
        Tone::Ok,
    )
}

/// An unknown, used, expired or stale link.
pub fn email_change_invalid(server_name: &str) -> String {
    render_message(
        server_name,
        "Link invalid or expired",
        "This confirmation link is invalid, was already used, or has expired.",
        "Your e-mail address did not change. You can ask for the change again in Scacelith.",
        Tone::Error,
    )
}

/// Another account took the address meanwhile.
pub fn email_change_taken(server_name: &str) -> String {
    render_message(
        server_name,
        "Address already used",
        "Another account now uses this e-mail address, so it cannot be given to yours.",
        "Your e-mail address did not change.",
        Tone::Error,
    )
}
