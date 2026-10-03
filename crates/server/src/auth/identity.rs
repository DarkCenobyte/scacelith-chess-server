//! Username and e-mail rules.
//!
//! Usernames: `USERNAME_MIN..=USERNAME_MAX` characters of `[A-Za-z0-9_-]`, starting with a letter
//! or a digit; reserved names (and names starting with a reserved prefix, against the
//! impersonation of staff and system accounts) are refused. Uniqueness ignores case (store).
//! E-mail addresses: plain ASCII addresses (no internationalised local part), at most 254
//! characters, a dotted domain; stored and compared in lower case.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::mail::is_valid_address;
use crate::security::encoding::{js_is_space, js_trim, utf16_len};

/// The pattern of a username, as `GET /info` publishes it.
pub const USERNAME_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_-]*$";

/// Names nobody may take (compared in lower case).
pub const RESERVED_USERNAMES: [&str; 52] = [
    "admin",
    "administrator",
    "root",
    "system",
    "sysop",
    "scacelith",
    "stockfish",
    "moderator",
    "mod",
    "mods",
    "staff",
    "support",
    "help",
    "helpdesk",
    "server",
    "official",
    "owner",
    "operator",
    "team",
    "security",
    "abuse",
    "postmaster",
    "webmaster",
    "hostmaster",
    "noreply",
    "no-reply",
    "info",
    "contact",
    "anonymous",
    "deleted",
    "unknown",
    "null",
    "undefined",
    "none",
    "guest",
    "everyone",
    "here",
    "console",
    "api",
    "www",
    "mail",
    "bot",
    "engine",
    "computer",
    "arbiter",
    "referee",
    "robot",
    "ai",
    "white",
    "black",
    "you",
    "me",
];

/// Prefixes of the names nobody may take (`admin2`, `deleted#12`...).
const RESERVED_PREFIXES: [&str; 6] = ["admin", "moderator", "scacelith", "stockfish", "sysop", "deleted"];

/// The length limits of a username (`USERNAME_MIN`, `USERNAME_MAX`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsernameRules {
    /// Fewest characters.
    pub min: usize,
    /// Most characters.
    pub max: usize,
}

impl UsernameRules {
    /// The limits of the configuration.
    pub fn from_config(config: &Config) -> UsernameRules {
        let clamp = |v: i64| usize::try_from(v.max(0)).unwrap_or(usize::MAX);
        UsernameRules { min: clamp(config.username_min), max: clamp(config.username_max) }
    }
}

/// True when `name` matches [`USERNAME_PATTERN`].
fn matches_pattern(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Checks a new username: `Err` with the English message of the refusal.
pub fn check_username(name: &str, rules: UsernameRules) -> Result<(), Cow<'static, str>> {
    let len = utf16_len(name);
    if len < rules.min || len > rules.max {
        return Err(Cow::Owned(format!("The username must have {} to {} characters.", rules.min, rules.max)));
    }
    if !matches_pattern(name) {
        return Err(Cow::Borrowed(
            "The username may only contain letters, digits, _ and -, and must start with a letter or a digit.",
        ));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_USERNAMES.contains(&lower.as_str()) || RESERVED_PREFIXES.iter().any(|p| lower.starts_with(p))
    {
        return Err(Cow::Borrowed("This username is reserved."));
    }
    Ok(())
}

/// The canonical form of an e-mail address: trimmed (JavaScript white space) and lower case.
pub fn normalize_email(email: &str) -> String {
    js_trim(email).to_lowercase()
}

/// [`normalize_email`] of an optional address (`""` for none), for comparisons.
pub fn normalize_opt_email(email: Option<&str>) -> String {
    email.map(normalize_email).unwrap_or_default()
}

/// An address with its local part hidden but its first character: `n***@example.org` (notices
/// about an address that must not be shown in full).
pub fn mask_email(email: &str) -> String {
    let s = js_trim(email);
    match s.rfind('@') {
        Some(at) if at > 0 => {
            let first = s.chars().next().expect("non-empty before the @");
            format!("{first}***{}", &s[at..])
        }
        _ => "***".to_owned(),
    }
}

/// Sanity check of an e-mail address (already normalised).
pub fn is_valid_email(email: &str) -> bool {
    if !is_valid_address(email) || email.len() > 254 {
        return false;
    }
    let domain = &email[email.find('@').map_or(0, |i| i + 1)..];
    domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !email.contains("..")
}

/// A username derived from a display name or an e-mail address (`""` when none fits): accents
/// dropped (NFKD without combining marks), white space runs as `_`, other characters dropped,
/// no leading `_` or `-`, at most `USERNAME_MAX` characters.
pub fn suggest_username(source: &str, rules: UsernameRules) -> String {
    let mut s = String::with_capacity(source.len());
    let mut in_space = false;
    for c in source.nfkd() {
        if ('\u{0300}'..='\u{036F}').contains(&c) {
            continue;
        }
        if js_is_space(c) {
            if !in_space {
                s.push('_');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            s.push(c);
        }
    }
    let trimmed = s.trim_start_matches(['_', '-']);
    // ASCII only by now: bytes are characters.
    let cut = &trimmed[..trimmed.len().min(rules.max)];
    if check_username(cut, rules).is_ok() { cut.to_owned() } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: UsernameRules = UsernameRules { min: 3, max: 20 };

    #[test]
    fn usernames() {
        assert_eq!(check_username("alice", RULES), Ok(()));
        assert_eq!(check_username("a_l-1", RULES), Ok(()));
        assert_eq!(check_username("9lives", RULES), Ok(()));
        let len = "The username must have 3 to 20 characters.";
        assert_eq!(check_username("ab", RULES).unwrap_err(), len);
        assert_eq!(check_username(&"a".repeat(21), RULES).unwrap_err(), len);
        assert_eq!(check_username("", RULES).unwrap_err(), len);
        let pattern = "The username may only contain letters, digits, _ and -, and must start with a letter or a digit.";
        for bad in ["_alice", "-alice", "al ice", "alïce", "al.ice", "al@ice"] {
            assert_eq!(check_username(bad, RULES).unwrap_err(), pattern, "{bad}");
        }
        for reserved in [
            "admin",
            "ADMIN",
            "Administrator",
            "admin2",
            "Moderator_x",
            "deleted1",
            "stockfish99",
            "you",
            "no-reply",
        ] {
            assert_eq!(
                check_username(reserved, RULES).unwrap_err(),
                "This username is reserved.",
                "{reserved}"
            );
        }
        assert_eq!(check_username("mediator", RULES), Ok(()), "mod is a name, not a prefix");
        // The length counts UTF-16 units, before the pattern.
        assert_eq!(check_username("😀😀", UsernameRules { min: 3, max: 20 }).unwrap_err(), pattern);
        assert_eq!(check_username("😀", UsernameRules { min: 3, max: 20 }).unwrap_err(), len);
    }

    #[test]
    fn emails() {
        assert_eq!(normalize_email("  Alice@Example.ORG\u{FEFF}"), "alice@example.org");
        assert_eq!(normalize_opt_email(None), "");
        assert_eq!(mask_email("nora@example.org"), "n***@example.org");
        assert_eq!(mask_email(" x@y.z "), "x***@y.z");
        assert_eq!(mask_email("@example.org"), "***");
        assert_eq!(mask_email("nobody"), "***");
        assert!(is_valid_email("alice@example.org"));
        assert!(is_valid_email("a.b+tag@mail.example.co.uk"));
        for bad in [
            "alice@localhost",
            "alice@.example.org",
            "alice@example.org.",
            "al..ice@example.org",
            "alice",
            "a@b@c.d",
            "",
        ] {
            assert!(!is_valid_email(bad), "{bad}");
        }
        assert!(!is_valid_email(&format!("{}@example.org", "a".repeat(65))));
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggest_username("Jérôme Dupont", RULES), "Jerome_Dupont");
        assert_eq!(suggest_username("  __Zoë  ", RULES), "Zoe_");
        assert_eq!(suggest_username("nora.smith", RULES), "norasmith");
        assert_eq!(suggest_username("李", RULES), "");
        assert_eq!(suggest_username("Admin Person", RULES), "");
        assert_eq!(suggest_username("a very long display name indeed", RULES), "a_very_long_display_");
        assert_eq!(suggest_username("ﬁne", RULES), "fine");
    }
}
