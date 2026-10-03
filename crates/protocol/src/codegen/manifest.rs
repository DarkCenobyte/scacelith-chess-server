//! Frozen manifests and the append-only rule.
//!
//! Every released minor is frozen as `protocol/frozen/v{proto}.{minor}.json`: the canonical
//! schema (prose left out), pretty-printed. The schema must then keep the wire of each frozen
//! minor:
//!
//! * the schema of a frozen minor never changes on the wire (any change needs a new minor);
//! * a later minor only appends: new message types in a published range; new fields at the end
//!   of a message; new values of open enums; new flag bits, close codes, capability bits;
//! * nothing frozen is removed, renamed, renumbered or retyped; a retired enum value stays
//!   reserved forever; structs, constants, ranges and closed enums never change;
//! * bounds of a client message may only widen (a server keeps accepting older clients); bounds
//!   of a server message may only narrow (older clients keep accepting it).

use serde_json::Value;

use super::canon;
use super::model::{Dir, Enum, Field, Schema};

/// File name of the manifest of a version.
pub fn file_name(protocol: u16, minor: u16) -> String {
    format!("v{protocol}.{minor}.json")
}

/// The manifest text of a schema.
pub fn render(schema: &Schema) -> String {
    let value: Value = serde_json::from_str(&schema.canonical).expect("canonical JSON parses");
    let mut text = serde_json::to_string_pretty(&value).expect("a value serializes");
    text.push('\n');
    text
}

/// Parses a manifest; the error names the file.
pub fn parse(name: &str, text: &str) -> Result<Schema, String> {
    Schema::parse_frozen(text).map_err(|e| format!("{name}: {}", e.join("; ")))
}

/// Checks the schema against the frozen manifests of its protocol (any order). The result lists
/// every violation.
pub fn check(schema: &Schema, frozen: &[Schema]) -> Vec<String> {
    let mut errors = Vec::new();
    for f in frozen.iter().filter(|f| f.protocol == schema.protocol) {
        let v = format!("v{}.{}", f.protocol, f.minor);
        if f.minor > schema.minor {
            errors.push(format!("{v} is frozen but the schema is at minor {}", schema.minor));
        } else if f.minor == schema.minor {
            if canon::canonical_json(&strip(&schema.canonical)) != canon::canonical_json(&strip(&f.canonical))
            {
                errors.push(format!(
                    "the wire of the frozen {v} changed: revert the change, or bump `minor` to {} for an append-only change",
                    schema.minor + 1
                ));
            }
        } else {
            append_only(schema, f, &v, &mut errors);
        }
    }
    errors
}

fn strip(canonical: &str) -> Value {
    serde_json::from_str(canonical).expect("canonical JSON parses")
}

fn append_only(new: &Schema, old: &Schema, v: &str, errors: &mut Vec<String>) {
    let mut err = |e: String| errors.push(format!("{v}: {e}"));
    if new.subprotocol != old.subprotocol {
        err("the subprotocol changed".into());
    }
    if canon_of(&new.ranges) != canon_of(&old.ranges) {
        err("the type ranges changed".into());
    }
    for c in &old.constants {
        if !new.constants.iter().any(|n| n.name == c.name && n.value == c.value) {
            err(format!("constant {} was changed or removed", c.name));
        }
    }
    for c in &old.caps {
        if !new.caps.iter().any(|n| n.name == c.name && n.bit == c.bit) {
            err(format!("capability {} was changed or removed", c.name));
        }
    }
    for c in &old.close_codes {
        if !new.close_codes.iter().any(|n| n.name == c.name && n.code == c.code && n.error == c.error) {
            err(format!("close code {} ({}) was changed or removed", c.name, c.code));
        }
    }
    for f in &old.flags {
        match new.flags.iter().find(|n| n.name == f.name) {
            None => err(format!("flag set {} was removed", f.name)),
            Some(n) => {
                for b in &f.bits {
                    if !n.bits.iter().any(|x| x.name == b.name && x.value == b.value) {
                        err(format!("flag {}.{} was changed or removed", f.name, b.name));
                    }
                }
            }
        }
    }
    for e in &old.enums {
        match new.enums.iter().find(|n| n.name == e.name) {
            None => err(format!("enum {} was removed", e.name)),
            Some(n) => enum_append_only(n, e, &mut err),
        }
    }
    for s in &old.structs {
        match new.structs.iter().find(|n| n.name == s.name) {
            Some(n) if same_fields(&n.fields, &s.fields) => {}
            _ => err(format!("struct {} changed (structs are frozen: they sit inside messages)", s.name)),
        }
    }
    for m in &old.messages {
        let Some(n) = new.messages.iter().find(|n| n.id == m.id) else {
            err(format!("message {:#04x} {} was removed", m.id, m.name));
            continue;
        };
        if n.name != m.name || n.dir != m.dir || n.relay_of != m.relay_of {
            err(format!("message {:#04x} {} was renamed or redirected", m.id, m.name));
        }
        if n.fields.len() < m.fields.len() {
            err(format!("{}: fields were removed", m.key));
            continue;
        }
        for (a, b) in n.fields.iter().zip(&m.fields) {
            if a.name != b.name || a.ty != b.ty {
                err(format!("{}.{}: renamed or retyped (fields may only be appended)", m.key, b.name));
            } else if !bounds_compatible(a, b, m.dir) {
                let rule = match m.dir {
                    Dir::C2s => "bounds of a client message may only widen",
                    Dir::S2c => "bounds of a server message may only narrow",
                };
                err(format!("{}.{}: {rule}", m.key, b.name));
            }
        }
    }
}

fn enum_append_only(new: &Enum, old: &Enum, err: &mut impl FnMut(String)) {
    if new.open != old.open {
        err(format!("enum {} changed between open and closed", old.name));
    }
    for v in &old.values {
        if new.by_value(v.value).map(|x| &x.name) != Some(&v.name) {
            err(format!("{}.{} ({}) was changed or removed", old.name, v.name, v.value));
        }
    }
    for v in &old.reserved {
        if !new.reserved.iter().any(|x| x.value == v.value) {
            err(format!("{} {} was reserved and must stay reserved", old.name, v.value));
        }
    }
    if !old.open && new.values.len() != old.values.len() {
        err(format!("closed enum {} gained values", old.name));
    }
}

fn same_fields(a: &[Field], b: &[Field]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| x.name == y.name && x.ty == y.ty && x.min == y.min && x.max == y.max)
}

/// Whether the bounds of `new` keep the compatibility of `old` in direction `dir`.
fn bounds_compatible(new: &Field, old: &Field, dir: Dir) -> bool {
    let (nl, nh, ol, oh) = (new.lo(), new.hi(), old.lo(), old.hi());
    match dir {
        Dir::C2s => nl <= ol && nh >= oh,
        Dir::S2c => nl >= ol && nh <= oh,
    }
}

fn canon_of(ranges: &[super::model::Range]) -> Vec<(u8, u8, Dir, String, bool)> {
    ranges.iter().map(|r| (r.first, r.last, r.dir, r.area.clone(), r.published)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{
        "protocol": 1, "minor": 0, "subprotocol": "t.rt1",
        "constants": [{"name": "MaxClientMessage", "value": 512}, {"name": "MaxServerMessage", "value": 65536}],
        "ranges": [{"first": 1, "last": 127, "dir": "c2s", "area": "all"}, {"first": 128, "last": 255, "dir": "s2c", "area": "all"}],
        "enums": [{"name": "ErrorCode", "open": true, "unknownDoc": "refuse", "values": [{"name": "Malformed", "value": 1}]}],
        "closeCodes": [{"name": "Malformed", "code": 4001, "error": "Malformed"}],
        "messages": [
            {"id": 1, "name": "Hello", "dir": "c2s", "fields": [{"name": "seq", "type": "u32"}, {"name": "proto", "type": "u16"},
                {"name": "minor", "type": "u16"}, {"name": "caps", "type": "u64"}, {"name": "n", "type": "u8", "max": 10}]},
            {"id": 2, "name": "Ping", "dir": "c2s", "fields": [{"name": "seq", "type": "u32"}, {"name": "nonce", "type": "u32"}]},
            {"id": 3, "name": "Pong", "dir": "c2s", "fields": [{"name": "seq", "type": "u32"}, {"name": "nonce", "type": "u32"}]},
            {"id": 130, "name": "Ping", "dir": "s2c", "fields": [{"name": "nonce", "type": "u32"}, {"name": "serverTime", "type": "f64"}]},
            {"id": 131, "name": "Pong", "dir": "s2c", "fields": [{"name": "nonce", "type": "u32"}, {"name": "serverTime", "type": "f64"}]},
            {"id": 128, "name": "Welcome", "dir": "s2c", "fields": [{"name": "proto", "type": "u16"}, {"name": "minor", "type": "u16"},
                {"name": "caps", "type": "u64"}, {"name": "n", "type": "u8", "max": 10}]},
            {"id": 129, "name": "Error", "dir": "s2c", "fields": [{"name": "ref", "type": "u32"}, {"name": "code", "type": "enum:ErrorCode"},
                {"name": "fatal", "type": "bool"}, {"name": "game", "type": "id53"}]}
        ]
    }"#;

    fn schema(edit: impl FnOnce(&mut Value)) -> Schema {
        let mut v: Value = serde_json::from_str(BASE).unwrap();
        edit(&mut v);
        Schema::parse(&v.to_string()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn identical_and_append_only_changes_pass() {
        let old = schema(|_| {});
        assert!(check(&old, std::slice::from_ref(&old)).is_empty());
        let reparsed = parse("v1.0.json", &render(&old)).unwrap();
        assert!(check(&old, &[reparsed]).is_empty());
        let new = schema(|v| {
            v["minor"] = 1.into();
            v["messages"][0]["fields"][4]["max"] = 20.into();
            v["messages"][5]["fields"][3]["max"] = 5.into();
            v["messages"][5]["fields"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": "m", "type": "u8"}));
            v["enums"][0]["values"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": "Other", "value": 2}));
        });
        assert_eq!(check(&new, &[old]), Vec::<String>::new());
    }

    #[test]
    fn breaking_changes_are_refused() {
        let old = schema(|_| {});
        let same_minor = schema(|v| v["messages"][5]["fields"][3]["max"] = 5.into());
        assert_eq!(check(&same_minor, std::slice::from_ref(&old)).len(), 1);
        let cases: [fn(&mut Value); 4] = [
            |v| v["messages"][0]["fields"][4]["max"] = 5.into(),
            |v| v["messages"][5]["fields"][3]["max"] = 20.into(),
            |v| v["messages"][5]["fields"][3]["name"] = "k".into(),
            |v| {
                v["messages"][5]["fields"].as_array_mut().unwrap().pop();
            },
        ];
        for edit in cases {
            let new = schema(|v| {
                v["minor"] = 1.into();
                edit(v);
            });
            assert!(!check(&new, std::slice::from_ref(&old)).is_empty());
        }
    }
}
