//! The e-mail confirmation page: `GET /verify-email?token=` shows a button (link scanners must not
//! use the token), `POST /verify-email` confirms the address (and creates the account of a pending
//! signup).

use super::layout::{Tone, escape_html, render_message, render_page};

/// The confirmation button.
pub fn verify_form(server_name: &str, token: &str) -> String {
    render_page(
        server_name,
        "Confirm your e-mail address",
        &format!(
            "<h1>Confirm your e-mail address</h1>\
             <p>Press the button to confirm this address for your account.</p>\
             <form method=\"post\" action=\"/verify-email\">\
             <input type=\"hidden\" name=\"token\" value=\"{}\">\
             <button type=\"submit\">Confirm my e-mail address</button></form>",
            escape_html(token)
        ),
    )
}

/// The address is confirmed.
pub fn verify_done(server_name: &str) -> String {
    render_message(
        server_name,
        "E-mail address confirmed",
        "Thank you, your e-mail address is confirmed.",
        "You can go back to Scacelith and log in.",
        Tone::Ok,
    )
}

/// Another account took the username or the address of a pending signup before its link was
/// used.
pub fn verify_taken(server_name: &str) -> String {
    render_message(
        server_name,
        "Account not created",
        "Another account took this username or this e-mail address before the link was used.",
        "Create your account again from Scacelith, with another username, or sign in if this address already has an account.",
        Tone::Error,
    )
}

/// An unknown, used or expired link.
pub fn verify_invalid(server_name: &str) -> String {
    render_message(
        server_name,
        "Link invalid or expired",
        "This confirmation link is invalid, was already used, or has expired.",
        "If you have just signed up, press \"Resend the e-mail\" on the page Scacelith shows after signing up, or create \
         your account again from Scacelith (the same username and address work), to receive a new link (at most one every \
         5 minutes). An existing account can ask for a new confirmation e-mail from the login screen of Scacelith.",
        Tone::Error,
    )
}
