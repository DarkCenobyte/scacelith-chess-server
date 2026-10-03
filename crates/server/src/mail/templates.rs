//! The plain-text English e-mails. They never contain a password or a session token;
//! verification and reset links carry single-use tokens that expire.

use super::message::utc_string;

/// A rendered e-mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The subject.
    pub subject: String,
    /// The body (`\n` line breaks).
    pub text: String,
}

/// An e-mail of the server, with its values. Times are Unix milliseconds, printed like
/// JavaScript's `toUTCString()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template<'a> {
    /// The confirmation link of a new account.
    Verification { username: &'a str, link: &'a str, hours: i64 },
    /// The password reset link.
    PasswordReset { username: &'a str, link: &'a str, minutes: i64 },
    /// To the owner of an address somebody tried to register again, or (`email_change`) that
    /// another player asked to move their account to.
    RegistrationAttempt { username: &'a str, email_change: bool },
    /// Two-step verification was turned off.
    MfaDisabled { username: &'a str, when_ms: i64 },
    /// The password was changed (or reset with an e-mail link).
    PasswordChanged { username: &'a str, when_ms: i64, by_reset: bool },
    /// To the new address of an e-mail change: the confirmation link.
    EmailChangeConfirm { username: &'a str, link: &'a str, hours: i64 },
    /// To the current address when a change of address is requested.
    EmailChangeRequested { username: &'a str, masked_email: &'a str, when_ms: i64, hours: i64 },
    /// To the former address once the change of address is done.
    EmailChanged { username: &'a str, masked_email: &'a str, when_ms: i64 },
    /// To the address of an account that Google sign-in just created.
    SsoAccountCreated { username: &'a str, when_ms: i64 },
    /// To the account's address when Google sign-in is added to an existing account.
    SsoLinked { username: &'a str, when_ms: i64 },
}

/// The signature of every e-mail.
fn sign(server_name: &str) -> String {
    format!("\n-- \n{server_name}\nThis is an automatic message; replies are not read.\n")
}

impl Template<'_> {
    /// The template's name (the `template` field of the mail logs).
    pub fn name(&self) -> &'static str {
        match self {
            Template::Verification { .. } => "verification",
            Template::PasswordReset { .. } => "passwordReset",
            Template::RegistrationAttempt { .. } => "registrationAttempt",
            Template::MfaDisabled { .. } => "mfaDisabled",
            Template::PasswordChanged { .. } => "passwordChanged",
            Template::EmailChangeConfirm { .. } => "emailChangeConfirm",
            Template::EmailChangeRequested { .. } => "emailChangeRequested",
            Template::EmailChanged { .. } => "emailChanged",
            Template::SsoAccountCreated { .. } => "ssoAccountCreated",
            Template::SsoLinked { .. } => "ssoLinked",
        }
    }

    /// The subject and body for the server `server_name`.
    pub fn render(&self, server_name: &str) -> Rendered {
        let s = server_name;
        let (subject, text) = match *self {
            Template::Verification { username, link, hours } => (
                format!("Confirm your e-mail address for {s}"),
                format!(
                    "Hello {username},\n\n\
                     Welcome to {s}. To confirm your e-mail address and start playing online, open\n\
                     this link and press the confirmation button:\n\n{link}\n\n\
                     The link is valid for {hours} hours. If you did not sign up, ignore this message:\n\
                     without the confirmation nothing is created or confirmed.\n{}",
                    sign(s)
                ),
            ),
            Template::PasswordReset { username, link, minutes } => (
                format!("Reset your {s} password"),
                format!(
                    "Hello {username},\n\n\
                     Someone (hopefully you) asked to reset the password of your {s} account.\n\
                     To choose a new password, open this link:\n\n{link}\n\n\
                     The link is valid for {minutes} minutes and can be used once. Resetting the password\n\
                     signs out every device. Two-step verification, if enabled, stays enabled.\n\n\
                     If you did not ask for this, ignore this message: your password does not change.\n{}",
                    sign(s)
                ),
            ),
            Template::RegistrationAttempt { username, email_change: true } => (
                format!("Someone tried to use your e-mail address on {s}"),
                format!(
                    "Hello {username},\n\n\
                     A player of {s} asked to change the e-mail address of their account to yours.\n\
                     Your address already belongs to your account, so it was not given to theirs, and your\n\
                     account did not change.\n\n\
                     You do not need to do anything.\n{}",
                    sign(s)
                ),
            ),
            Template::RegistrationAttempt { username, email_change: false } => (
                format!("Someone tried to register on {s} with your e-mail address"),
                format!(
                    "Hello {username},\n\n\
                     Someone tried to create a new {s} account with your e-mail address. Your\n\
                     address already belongs to your account, so no new account was created.\n\n\
                     If it was you, you can simply log in. If you forgot your password, use \"Forgot\n\
                     password\" in the game to receive a reset link.\n\n\
                     If it was not you, you do not need to do anything.\n{}",
                    sign(s)
                ),
            ),
            Template::MfaDisabled { username, when_ms } => (
                format!("Two-step verification was turned off on {s}"),
                format!(
                    "Hello {username},\n\n\
                     Two-step verification (authenticator codes) was turned off for your {s}\n\
                     account on {}.\n\n\
                     If you did not do this, reset your password at once from the game (\"Forgot password\"),\n\
                     then turn two-step verification on again.\n{}",
                    utc_string(when_ms),
                    sign(s)
                ),
            ),
            Template::PasswordChanged { username, when_ms, by_reset } => (
                format!("Your {s} password was changed"),
                format!(
                    "Hello {username},\n\n\
                     The password of your {s} account was {} on\n\
                     {}. {}\n\n\
                     If you did not do this, reset your password at once from the game (\"Forgot password\").\n{}",
                    if by_reset { "reset with an e-mail link" } else { "changed" },
                    utc_string(when_ms),
                    if by_reset {
                        "Every device was signed out."
                    } else {
                        "Your other devices were signed out."
                    },
                    sign(s)
                ),
            ),
            Template::EmailChangeConfirm { username, link, hours } => (
                format!("Confirm your new e-mail address for {s}"),
                format!(
                    "Hello {username},\n\n\
                     You asked to use this e-mail address for your {s} account. To confirm it, open\n\
                     this link and press the confirmation button:\n\n{link}\n\n\
                     The link is valid for {hours} hours and can be used once. Until it is used, your account\n\
                     keeps its current address.\n\n\
                     If you did not ask for this, ignore this message: nothing changes.\n{}",
                    sign(s)
                ),
            ),
            Template::EmailChangeRequested { username, masked_email, when_ms, hours } => (
                format!("A change of your {s} e-mail address was requested"),
                format!(
                    "Hello {username},\n\n\
                     On {}, a change of the e-mail address of your {s} account\n\
                     to {masked_email} was requested, with your password. The address changes only if the\n\
                     link sent to the new address is opened within {hours} hours; until then, this address stays\n\
                     the one of your account.\n\n\
                     If it was you, there is nothing else to do.\n\n\
                     If it was not you, someone knows your password: change it at once in the game, or reset it\n\
                     with \"Forgot password\". A new password cancels the change of address.\n{}",
                    utc_string(when_ms),
                    sign(s)
                ),
            ),
            Template::EmailChanged { username, masked_email, when_ms } => (
                format!("Your {s} e-mail address was changed"),
                format!(
                    "Hello {username},\n\n\
                     The e-mail address of your {s} account was changed to {masked_email} on\n\
                     {}.\n\n\
                     Messages about your account, password resets included, now go to the new address: this\n\
                     is the last one sent to this address.\n\n\
                     If you did not do this, someone else controls your account: contact the administrator of\n\
                     {s} at once.\n{}",
                    utc_string(when_ms),
                    sign(s)
                ),
            ),
            Template::SsoAccountCreated { username, when_ms } => (
                format!("A {s} account was created with your Google account"),
                format!(
                    "Hello {username},\n\n\
                     The {s} account \"{username}\" was created with Google sign-in, with the Google\n\
                     account of this address, on {}.\n\n\
                     If it was you, there is nothing else to do.\n\n\
                     If it was not you, someone else may be signed in to it: sign in with Google in the game,\n\
                     use \"Sign out everywhere\" on the account page, and contact the administrator of {s}.\n\
                     Never send anyone the address your browser shows after a sign-in.\n{}",
                    utc_string(when_ms),
                    sign(s)
                ),
            ),
            Template::SsoLinked { username, when_ms } => (
                format!("Google sign-in was added to your {s} account"),
                format!(
                    "Hello {username},\n\n\
                     Google sign-in was added to your {s} account \"{username}\" on\n\
                     {}, with your password. From now on, the Google account of this address\n\
                     signs in to it without the password.\n\n\
                     If it was you, there is nothing else to do.\n\n\
                     If it was not you, someone knows your password: change it at once in the game, or reset it\n\
                     with \"Forgot password\", then use \"Sign out everywhere\" on the account page and contact the\n\
                     administrator of {s}.\n{}",
                    utc_string(when_ms),
                    sign(s)
                ),
            ),
        };
        Rendered { subject, text }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::message::tests::{WHEN, vectors};

    const S: &str = "Scacelith Test Server";

    #[test]
    fn templates_match_the_former_server() {
        let cases = [
            (
                "verification",
                Template::Verification {
                    username: "alice",
                    link: "https://play.example:8443/verify-email?token=AbC",
                    hours: 24,
                },
            ),
            (
                "passwordReset",
                Template::PasswordReset {
                    username: "alice",
                    link: "https://play.example/reset-password?token=xyz",
                    minutes: 60,
                },
            ),
            ("registrationAttempt", Template::RegistrationAttempt { username: "bob", email_change: false }),
            (
                "registrationAttemptEmailChange",
                Template::RegistrationAttempt { username: "bob", email_change: true },
            ),
            ("mfaDisabled", Template::MfaDisabled { username: "carol", when_ms: WHEN }),
            (
                "passwordChangedReset",
                Template::PasswordChanged { username: "dave", when_ms: WHEN, by_reset: true },
            ),
            ("passwordChanged", Template::PasswordChanged { username: "dave", when_ms: 0, by_reset: false }),
            (
                "emailChangeConfirm",
                Template::EmailChangeConfirm {
                    username: "erin",
                    link: "https://h/confirm-email-change?token=T",
                    hours: 24,
                },
            ),
            (
                "emailChangeRequested",
                Template::EmailChangeRequested {
                    username: "erin",
                    masked_email: "n***@example.org",
                    when_ms: WHEN,
                    hours: 24,
                },
            ),
            (
                "emailChanged",
                Template::EmailChanged { username: "erin", masked_email: "n***@example.org", when_ms: WHEN },
            ),
            ("ssoAccountCreated", Template::SsoAccountCreated { username: "frank", when_ms: WHEN }),
            ("ssoLinked", Template::SsoLinked { username: "grace", when_ms: WHEN }),
        ];
        let v = vectors();
        for (key, t) in cases {
            let r = t.render(S);
            assert_eq!(r.subject, v["templates"][key]["subject"].as_str().unwrap(), "{key}");
            assert_eq!(r.text, v["templates"][key]["text"].as_str().unwrap(), "{key}");
        }
        assert_eq!(v["templates"].as_object().unwrap().len(), 12);
    }

    #[test]
    fn subjects_links_and_names() {
        let v =
            Template::Verification { username: "alice", link: "https://h/verify-email?token=T", hours: 24 }
                .render("S");
        assert!(v.subject.contains("Confirm your e-mail address"));
        assert!(v.text.contains("https://h/verify-email?token=T"));
        let r = Template::PasswordReset { username: "a", link: "L", minutes: 60 }.render("S");
        assert!(r.text.contains("valid for 60 minutes"));
        let names = [
            Template::Verification { username: "", link: "", hours: 0 }.name(),
            Template::PasswordReset { username: "", link: "", minutes: 0 }.name(),
            Template::RegistrationAttempt { username: "", email_change: true }.name(),
            Template::MfaDisabled { username: "", when_ms: 0 }.name(),
            Template::PasswordChanged { username: "", when_ms: 0, by_reset: false }.name(),
            Template::EmailChangeConfirm { username: "", link: "", hours: 0 }.name(),
            Template::EmailChangeRequested { username: "", masked_email: "", when_ms: 0, hours: 0 }.name(),
            Template::EmailChanged { username: "", masked_email: "", when_ms: 0 }.name(),
            Template::SsoAccountCreated { username: "", when_ms: 0 }.name(),
            Template::SsoLinked { username: "", when_ms: 0 }.name(),
        ];
        assert_eq!(
            names,
            [
                "verification",
                "passwordReset",
                "registrationAttempt",
                "mfaDisabled",
                "passwordChanged",
                "emailChangeConfirm",
                "emailChangeRequested",
                "emailChanged",
                "ssoAccountCreated",
                "ssoLinked"
            ]
        );
    }
}
