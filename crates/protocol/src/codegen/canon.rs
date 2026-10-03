//! Canonical JSON of the schema and its fingerprint.
//!
//! The canonical form keeps the wire part of the schema only: keys named `about` or `doc`, or
//! ending in `Doc`, are left out at every level; object keys are sorted (byte order), arrays keep
//! their order, numbers are integers, there is no whitespace. The fingerprint is the first four
//! bytes (big-endian) of SHA-256 of that text: informational (vectors, `/api/v1/info`, logs),
//! never a reason to refuse a peer.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Whether a key holds prose (left out of the canonical form).
pub fn is_prose_key(key: &str) -> bool {
    key == "about" || key == "doc" || key.ends_with("Doc")
}

/// The canonical JSON text of a schema value.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().filter(|k| !is_prose_key(k)).collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a string serializes"));
                out.push(':');
                write(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// First four bytes (big-endian) of SHA-256 of the canonical text.
pub fn fingerprint(canonical: &str) -> u32 {
    let digest = Sha256::digest(canonical.as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_is_left_out_and_keys_sorted() {
        let v: Value =
            serde_json::from_str(r#"{"b":1,"a":[{"doc":"x","n":"é"}],"unknownDoc":"y","about":"z"}"#)
                .unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":[{"n":"é"}],"b":1}"#);
        // SHA-256("abc") = ba7816bf...
        assert_eq!(fingerprint("abc"), 0xba78_16bf);
    }
}
