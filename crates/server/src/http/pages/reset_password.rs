//! The password reset page: `GET /reset-password?token=` shows the form, `POST /reset-password`
//! sets the new password (the token travels in the form; it is single use and expires after an
//! hour).

use super::layout::{Tone, escape_html, render_message, render_page};
use crate::security::password::PASSWORD_MAX_BYTES;

/// The form of a new password, with an error above it when one is given.
pub fn reset_form(server_name: &str, token: &str, min_length: i64, error: Option<&str>) -> String {
    let error = error.filter(|e| !e.is_empty()).map(|e| format!("<p class=\"error\">{}</p>", escape_html(e)));
    render_page(
        server_name,
        "Choose a new password",
        &format!(
            "<h1>Choose a new password</h1>{}\
             <p>At least {min_length} characters. Every device signed in to your account will be signed out.</p>\
             <form method=\"post\" action=\"/reset-password\">\
             <input type=\"hidden\" name=\"token\" value=\"{}\">\
             <label for=\"np\">New password</label><input id=\"np\" type=\"password\" name=\"newPassword\" \
             autocomplete=\"new-password\" minlength=\"{min_length}\" maxlength=\"{PASSWORD_MAX_BYTES}\" required>\
             <label for=\"cp\">Repeat the new password</label><input id=\"cp\" type=\"password\" name=\"confirmPassword\" \
             autocomplete=\"new-password\" minlength=\"{min_length}\" maxlength=\"{PASSWORD_MAX_BYTES}\" required>\
             <button type=\"submit\">Change my password</button></form>",
            error.unwrap_or_default(),
            escape_html(token)
        ),
    )
}

/// The password changed.
pub fn reset_done(server_name: &str) -> String {
    render_message(
        server_name,
        "Password changed",
        "Your password has been changed and every device was signed out.",
        "You can go back to Scacelith and log in with your new password.",
        Tone::Ok,
    )
}

/// An unknown, used or expired link.
pub fn reset_invalid(server_name: &str) -> String {
    render_message(
        server_name,
        "Link invalid or expired",
        "This reset link is invalid, was already used, or has expired.",
        "You can ask for a new one with \"Forgot password\" in Scacelith.",
        Tone::Error,
    )
}
