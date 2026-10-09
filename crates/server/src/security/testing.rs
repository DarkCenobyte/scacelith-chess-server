//! Credentials of the unit tests. No password, key or salt of the tests is written in their
//! sources: a password whose value does not matter is drawn at random when the tests run
//! ([`random_password`]), and a value a test checks (a published test vector, a password of the
//! policy tests) is read from `test/fixtures/security-vectors.json` ([`vector`]).

use std::path::Path;
use std::sync::LazyLock;

use serde_json::Value;

use super::encoding::random_bytes;

/// A new password from the operating system's random generator: four groups of six decimal
/// digits. It passes the password policy and holds none of the user names and e-mail addresses
/// of the tests, which have letters.
pub fn random_password() -> String {
    let r = random_bytes::<16>();
    let group =
        |i: usize| u32::from_le_bytes([r[4 * i], r[4 * i + 1], r[4 * i + 2], r[4 * i + 3]]) % 1_000_000;
    format!("{:06} {:06} {:06} {:06}", group(0), group(1), group(2), group(3))
}

/// The string at `pointer` (a JSON pointer such as `/passwords/short`) in
/// `test/fixtures/security-vectors.json`.
///
/// # Panics
/// When the file cannot be read or holds no string there.
pub fn vector(pointer: &str) -> &'static str {
    static VECTORS: LazyLock<Value> = LazyLock::new(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures/security-vectors.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    });
    VECTORS.pointer(pointer).and_then(Value::as_str).unwrap_or_else(|| panic!("no string at {pointer}"))
}
