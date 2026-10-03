//! Small helpers shared by several modules.
//!
//! * [`js`]: JavaScript-compatible number formatting (`String(n)`), `Math.round`, `toFixed`,
//!   `trim` and UTF-16 lengths, so answers and logs read as the former Node.js server's did.
//! * [`json`]: `JSON.stringify` (compact and two-space pretty) over `serde_json::Value`, with
//!   JavaScript number formatting (`1`, not `1.0`).
//! * [`encoding`]: base64url without padding, standard base64, Node's lenient base64 decoder.
//! * [`hash`]: SHA-256, HMAC-SHA-256, constant-time comparison.
//! * [`random`]: random bytes, tokens, UUID v4, unbiased integers (system generator).
//! * [`ip`]: client address normalisation and address/CIDR lists.
//! * [`path`]: `path.resolve` and `path.join` (lexical).
//! * [`errno`]: `ENOENT`-style names of I/O errors.
//! * [`time`]: the `Date.prototype.toUTCString` format.

pub mod encoding;
pub mod errno;
pub mod hash;
pub mod ip;
pub mod js;
pub mod json;
pub mod path;
pub mod random;
pub mod time;

pub use encoding::{base64_decode_lenient, base64url_decode, base64url_encode};
pub use hash::{ct_eq, ct_eq_hashed, hmac_sha256, sha256};
pub use js::{number_to_string as js_number, round as js_round, trim as js_trim, utf16_len};
pub use random::uuid_v4;
