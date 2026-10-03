//! Golden vectors: `test/fixtures/protocol-vectors.json`, read by the crate's tests, the
//! server's tests and the game client's C++ tests.
//!
//! Every vector is built and checked with the interpreted codec (`interp`), never with the
//! generated codecs it tests. File format (one vector per line, deterministic):
//!
//! * `valid[]`: `{name, type, dir, note, fields, hex}`: encoding `fields` gives `hex`, decoding
//!   `hex` gives `fields` (enums as numbers, structs as objects, lists as arrays).
//! * `malformed[]`: `{name, type, dir, note, reason, hex}`: every receiver in direction `dir`
//!   refuses `hex`, lenient or not; `reason` is the stable reason of the Rust codec. Each has
//!   exactly one defect, so every correct decoder refuses it whatever order it checks in.
//! * `lenient[]`: `{name, type, dir, note, hex, fields, strictReason}`: a lenient receiver (a
//!   client for `s2c`; a server reading a Hello for `c2s`) decodes `hex` as `fields`, or ignores
//!   it when `fields` is null; a strict decoder refuses it with `strictReason`.
//! * `fnv1a32[]`: `{text, hash}`; `moves[]`: `{uci, value}` (`value` null: not a move).

use serde_json::{Map, Value, json};

use super::interp::{self, Decoded, Rng};
use super::model::{Dir, Field, Schema, Type};
use crate::moves::{fnv1a32, move_to_uci, uci_to_move};

/// A realistic game id (ms since 2026-01-01 << 12 | shard << 6 | sequence).
const GAME: u64 = 95_728_435_200_197;
/// A server wall-clock time (epoch ms, with a fraction).
const T0: f64 = 1_790_000_000_123.5;
/// A session token (`sct_` + base64url of 32 bytes).
const TOKEN: &str = "sct_CzBVep_E6Q4zWH2ix-wRNluApcrvFDleg6jN8hc8YYY";
/// 2^53 - 1, the largest id53 (and the largest integer a JSON reader keeps exactly).
const MAX53: u64 = (1 << 53) - 1;
const U32: u64 = 0xffff_ffff;
const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -";

fn mv(uci: &str) -> u16 {
    uci_to_move(uci).unwrap_or_else(|| panic!("vectors: {uci} is not a move"))
}

fn player(user_id: u64, name: &str, rating: u64, provisional: bool) -> Value {
    json!({ "userId": user_id, "name": name, "rating": rating, "provisional": provisional })
}

/// `n` random move records (deterministic).
fn move_list(n: usize, seed: u64) -> Value {
    let mut rng = Rng::new(seed);
    let (mut white, mut black) = (180_000i64, 180_000i64);
    let moves = (0..n)
        .map(|i| {
            let promo = if rng.below(50) == 0 { 2 + rng.below(4) } else { 0 };
            let m = rng.below(64) | rng.below(64) << 6 | promo << 12;
            let spent = if i < 2 { 0 } else { rng.below(9000) as i64 };
            let clock = if i % 2 == 0 { &mut white } else { &mut black };
            *clock = (*clock - spent + 2000).max(0);
            json!({ "move": m, "spentMs": spent, "clockMs": *clock })
        })
        .collect();
    Value::Array(moves)
}

/// A GameSnapshot after 5.O-O Be7 of a Ruy Lopez, with fields of `over` replaced.
fn snapshot(over: Value) -> Value {
    let ruy = ["e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6", "b5a4", "g8f6", "e1g1", "f8e7"];
    let (mut white, mut black) = (180_000i64, 180_000i64);
    let moves: Vec<Value> = ruy
        .iter()
        .enumerate()
        .map(|(i, uci)| {
            let spent = if i < 2 { 0 } else { 1500 + 311 * i as i64 };
            let clock = if i % 2 == 0 { &mut white } else { &mut black };
            *clock += if i < 2 { 0 } else { 2000 - spent };
            json!({ "move": mv(uci), "spentMs": spent, "clockMs": *clock })
        })
        .collect();
    let mut s = json!({
        "game": GAME, "gseq": 12, "category": "3+2", "baseMs": 180_000, "incMs": 2000, "rated": true,
        "white": player(1017, "Łukasz", 1532, false), "black": player(2048, "ユキ", 1498, true), "you": 0,
        "moves": moves, "running": 0, "whiteMs": white, "blackMs": black, "serverTime": T0, "drawOffer": 2,
        "status": 0, "reason": 0, "whiteConnected": true, "blackConnected": true, "graceMs": 18_000,
        "firstMoveMs": 0, "startedAt": T0 - 95_000.25, "rematch": 2, "autoPress": true,
    });
    merge(&mut s, over);
    s
}

/// `base` with the fields of `over` replaced.
fn merge(base: &mut Value, over: Value) {
    if let (Some(b), Value::Object(o)) = (base.as_object_mut(), over) {
        for (k, v) in o {
            assert!(b.contains_key(&k), "vectors: no field {k} to replace");
            b.insert(k, v);
        }
    }
}

/// `base` with the fields of `over` replaced, as a new value.
fn with(base: &Value, over: Value) -> Value {
    let mut v = base.clone();
    merge(&mut v, over);
    v
}

/// The typical value of each message (the vectors' reference sample).
fn typical(schema: &Schema, key: &str) -> Value {
    let e = |name: &str, value: &str| schema.enum_(name).by_name(value).map(|v| v.value).expect("enum value");
    match key {
        "Hello" => {
            json!({ "seq": 1, "proto": 1, "minor": 0, "caps": 0, "client": "Scacelith/1.4.0 (Windows x64)", "token": TOKEN })
        }
        "C_Ping" => json!({ "seq": 7, "nonce": 123_456 }),
        "C_Pong" => json!({ "seq": 8, "nonce": 0xdead_beefu32 }),
        "QueueJoin" => json!({ "seq": 2, "category": "3+2", "rated": true }),
        "QueueLeave" => json!({ "seq": 3 }),
        "ChallengeCreate" => {
            json!({ "seq": 4, "target": "yuki", "baseSec": 300, "incSec": 3, "rated": true, "color": e("ColorPref", "White") })
        }
        "ChallengeAccept" => json!({ "seq": 5, "id": 77 }),
        "ChallengeDecline" => json!({ "seq": 5, "id": 78 }),
        "ChallengeCancel" => json!({ "seq": 6, "id": 79 }),
        "ChallengeJoinCode" => json!({ "seq": 4, "code": "K7QX-9M2P" }),
        "Move" => json!({
            "seq": 12, "game": GAME, "ply": 0, "move": mv("e2e4"), "posHash": fnv1a32(START.as_bytes()),
            "thinkMs": 2345, "drawOffer": false,
        }),
        "Resign" => json!({ "seq": 20, "game": GAME }),
        "DrawOffer" => json!({ "seq": 21, "game": GAME }),
        "DrawAnswer" => json!({ "seq": 22, "game": GAME, "accept": true }),
        "DrawClaim" => json!({ "seq": 23, "game": GAME }),
        "Abort" => json!({ "seq": 9, "game": GAME }),
        "Resync" => json!({ "seq": 10, "game": GAME }),
        "Rematch" => json!({ "seq": 30, "game": GAME, "accept": true }),
        "C_Gesture" => json!({
            "seq": 31, "game": GAME, "ply": 6, "touch": 5, "aim": 26, "placed": 0, "flags": 0, "yaw": -212,
            "pitch": -598, "lean": 35,
        }),
        "Welcome" => json!({
            "proto": 1, "minor": 0, "caps": 0, "serverTime": T0, "userId": 1017, "username": "Łukasz",
            "serverName": "Scacelith Community Server", "heartbeatMs": 10_000, "clientPingMs": 10_000,
            "maxMsgPerSec": 20, "msgBurst": 40, "activeGame": 0, "gestureRate": 4, "gestureBurst": 8,
            "gestureIdleMs": 1000,
        }),
        "Error" => json!({ "ref": 12, "code": e("ErrorCode", "IllegalMove"), "fatal": false, "game": GAME }),
        "S_Ping" => json!({ "nonce": 991, "serverTime": T0 }),
        "S_Pong" => json!({ "nonce": 123_456, "serverTime": T0 + 12.25 }),
        "Ack" => json!({ "ref": 3 }),
        "Notice" => json!({ "code": e("NoticeCode", "ServerShutdown"), "arg": 30_000.0 }),
        "QueueStatus" => json!({
            "category": "3+2", "rated": true, "state": e("QueueState", "Searching"), "waitMs": 12_500,
            "window": 150, "queued": 42,
        }),
        "ChallengeReceived" => json!({
            "id": 77, "from": player(2048, "ユキ", 1612, false), "baseSec": 300, "incSec": 3, "rated": true,
            "yourColor": e("ColorPref", "Black"), "expiresMs": 60_000,
        }),
        "ChallengeStatus" => json!({
            "id": 78, "state": e("ChallengeState", "Pending"), "target": "", "code": "K7QX-9M2P", "baseSec": 600,
            "incSec": 5, "rated": false,
        }),
        "GameSnapshot" => snapshot(json!({})),
        "MoveMade" => json!({
            "game": GAME, "gseq": 5, "ply": 4, "move": mv("f1b5"), "flags": 0, "spentMs": 2744, "whiteMs": 177_256,
            "blackMs": 176_100, "serverTime": T0, "drawOffer": false, "firstMoveMs": 0,
        }),
        "MoveRejected" => {
            json!({ "game": GAME, "ply": 6, "move": mv("b5a4"), "code": e("ErrorCode", "Desync") })
        }
        "GameEvent" => json!({
            "game": GAME, "gseq": 9, "kind": e("GameEventKind", "PlayerDisconnected"), "color": 1, "arg": 18_000,
        }),
        "GameEnd" => json!({
            "game": GAME, "gseq": 61, "status": e("GameStatus", "WhiteWins"), "reason": e("EndReason", "Checkmate"),
            "whiteMs": 41_250, "blackMs": 3999, "serverTime": T0 + 600_000.0,
        }),
        "RatingUpdate" => json!({
            "game": GAME, "category": "3+2",
            "white": { "before": 1532, "after": 1548, "games": 31, "provisional": false },
            "black": { "before": 1498, "after": 1482, "games": 12, "provisional": true },
        }),
        "S_Gesture" => json!({
            "game": GAME, "ply": 6, "touch": 5, "aim": 26, "placed": 0, "flags": 0, "yaw": -212, "pitch": -598,
            "lean": 35,
        }),
        _ => {
            let m = schema.message(key).unwrap_or_else(|| panic!("vectors: no message {key}"));
            sample_fields(schema, &m.fields)
        }
    }
}

/// Plain values for the fields of a message added after these tables were written.
fn sample_fields(schema: &Schema, fields: &[Field]) -> Value {
    let mut map = Map::new();
    for f in fields {
        map.insert(f.name.clone(), sample(schema, f, &f.ty));
    }
    Value::Object(map)
}

fn sample(schema: &Schema, f: &Field, ty: &Type) -> Value {
    match ty {
        Type::Bool => Value::from(true),
        Type::F64 => Value::from(T0),
        Type::Id53 => Value::from(GAME),
        Type::U64 => Value::from(0),
        Type::Str8 => Value::from("x".repeat(f.lo().max(f.hi().min(8)) as usize)),
        Type::Enum(name) => Value::from(schema.enum_(name).values[0].value),
        Type::Struct(name) => sample_fields(schema, &schema.struct_(name).fields),
        Type::List(item) => {
            let item_field = Field {
                name: f.name.clone(),
                ty: (**item).clone(),
                min: None,
                max: None,
                doc: String::new(),
            };
            Value::Array(vec![sample(schema, &item_field, item)])
        }
        _ if f.name == "seq" => Value::from(1),
        _ => Value::from(42.clamp(f.lo(), f.hi())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// JSON pointer of a dotted field path.
fn pointer(path: &str) -> String {
    path.split('.').map(|p| format!("/{p}")).collect()
}

struct Builder<'a> {
    schema: &'a Schema,
    valid: Vec<Value>,
    malformed: Vec<Value>,
    lenient: Vec<Value>,
}

impl<'a> Builder<'a> {
    fn typical(&self, key: &str) -> Value {
        typical(self.schema, key)
    }

    fn enc(&self, key: &str, fields: &Value) -> Vec<u8> {
        let m = self.schema.message(key).unwrap_or_else(|| panic!("vectors: no message {key}"));
        interp::encode(self.schema, m, fields).unwrap_or_else(|e| panic!("vectors: cannot encode {key}: {e}"))
    }

    fn offset(&self, key: &str, fields: &Value, path: &str) -> usize {
        interp::locate(self.schema, self.schema.message(key).expect("message"), fields, path)
    }

    /// A valid vector.
    fn ok(&mut self, key: &str, fields: Value, note: &str) {
        let m = self.schema.message(key).unwrap_or_else(|| panic!("vectors: no message {key}"));
        let bytes = self.enc(key, &fields);
        let back = match interp::decode(self.schema, &bytes, m.dir, true) {
            Ok(Decoded::Message { key: k, fields }) if k == key => fields,
            other => panic!("vectors: \"{note}\" ({key}) decodes as {other:?}"),
        };
        assert_eq!(self.enc(key, &back), bytes, "vectors: \"{note}\" ({key}) does not round-trip");
        self.valid.push(json!({
            "name": key, "type": m.id, "dir": m.dir.as_str(), "note": note, "fields": back, "hex": hex(&bytes),
        }));
    }

    /// Name and type of the type byte of `bytes` (null when empty or unknown).
    fn type_of(&self, bytes: &[u8]) -> (Value, Value) {
        match bytes.first().and_then(|&t| self.schema.message_by_id(t)) {
            Some(m) => (Value::from(m.key.clone()), Value::from(m.id)),
            None => (Value::Null, Value::Null),
        }
    }

    /// A malformed vector: refused by every receiver in direction `dir` with `reason`.
    fn bad(&mut self, bytes: Vec<u8>, dir: Dir, reason: &str, note: &str) {
        for strict in [true, false] {
            match interp::decode(self.schema, &bytes, dir, strict) {
                Err(r) if r == reason => {}
                Err(r) => panic!("vectors: \"{note}\" fails with \"{r}\", expected \"{reason}\""),
                Ok(d) => panic!("vectors: \"{note}\" was accepted (strict: {strict}): {d:?}"),
            }
        }
        if dir == Dir::C2s && bytes.first() == Some(&1) && interp::decode_hello(self.schema, &bytes).is_ok() {
            panic!("vectors: \"{note}\" is accepted by the Hello reader");
        }
        let (name, ty) = self.type_of(&bytes);
        self.malformed.push(json!({
            "name": name, "type": ty, "dir": dir.as_str(), "note": note, "reason": reason, "hex": hex(&bytes),
        }));
    }

    /// A vector that lenient receivers accept (or ignore) and strict ones refuse.
    fn lenient(&mut self, bytes: Vec<u8>, dir: Dir, note: &str) {
        let strict_reason = match interp::decode(self.schema, &bytes, dir, true) {
            Err(r) => r,
            Ok(d) => panic!("vectors: \"{note}\" is accepted by a strict decoder: {d:?}"),
        };
        let fields = match dir {
            Dir::S2c => match interp::decode(self.schema, &bytes, dir, false) {
                Ok(Decoded::Message { fields, .. }) => fields,
                Ok(Decoded::Ignored) => Value::Null,
                Err(r) => panic!("vectors: \"{note}\" is refused by a lenient decoder: {r}"),
            },
            Dir::C2s => interp::decode_hello(self.schema, &bytes)
                .unwrap_or_else(|r| panic!("vectors: \"{note}\" is refused by the Hello reader: {r}")),
        };
        let (name, ty) = self.type_of(&bytes);
        self.lenient.push(json!({
            "name": name, "type": ty, "dir": dir.as_str(), "note": note, "hex": hex(&bytes), "fields": fields,
            "strictReason": strict_reason,
        }));
    }

    /// The encoding of `fields` with `raw` written at the offset of field `path`.
    fn patch(&self, key: &str, fields: &Value, path: &str, raw: &[u8]) -> Vec<u8> {
        let mut bytes = self.enc(key, fields);
        let at = self.offset(key, fields, path);
        bytes[at..at + raw.len()].copy_from_slice(raw);
        bytes
    }

    /// The encoding of `fields` with the str8 at `path` holding the bytes `raw` (any bytes).
    fn str_bytes(&self, key: &str, fields: &Value, path: &str, raw: &[u8]) -> Vec<u8> {
        let mut f = fields.clone();
        *f.pointer_mut(&pointer(path)).expect("field") = Value::from("z".repeat(raw.len()));
        let mut bytes = self.enc(key, &f);
        let at = self.offset(key, &f, path) + 1;
        bytes[at..at + raw.len()].copy_from_slice(raw);
        bytes
    }

    /// The encoding of `fields` with the str8 at `path` replaced by a length byte and `raw`.
    fn str_splice(&self, key: &str, fields: &Value, path: &str, raw: &[u8]) -> Vec<u8> {
        let bytes = self.enc(key, fields);
        let at = self.offset(key, fields, path);
        let len = u8::try_from(raw.len()).expect("a str8 payload");
        let mut out = bytes[..at].to_vec();
        out.push(len);
        out.extend_from_slice(raw);
        out.extend_from_slice(&bytes[at + 1 + usize::from(bytes[at])..]);
        out
    }
}

/// Builds the vectors file.
pub fn build(schema: &Schema) -> String {
    let mut b = Builder { schema, valid: Vec::new(), malformed: Vec::new(), lenient: Vec::new() };
    let e = |name: &str, value: &str| schema.enum_(name).by_name(value).map(|v| v.value).expect("enum value");

    for m in &schema.messages {
        let fields = b.typical(&m.key);
        b.ok(&m.key, fields, "typical");
    }
    edge_cases(&mut b, &e);
    for m in &schema.messages {
        assert!(b.valid.iter().any(|v| v["name"] == m.key.as_str()), "vectors: no vector for {}", m.key);
    }
    malformed(&mut b);
    lenient(&mut b, &e);

    let fnv: Vec<Value> = [
        "",
        START,
        "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq -",
        "r3k2r/8/8/3pP3/8/8/8/R3K2R w KQkq d6",
        "8/8/8/8/8/8/8/K6k w - -",
    ]
    .iter()
    .map(|text| json!({ "text": text, "hash": fnv1a32(text.as_bytes()) }))
    .collect();
    let moves: Vec<Value> =
        ["e2e4", "e7e8q", "e1g1", "e8c8", "a1a1", "h8h8", "a7a8n", "h2h1b", "b7b8r", "e2e9", "e7e8k", "e2e"]
            .iter()
            .map(|uci| {
                let value = uci_to_move(uci);
                if let Some(m) = value {
                    assert_eq!(move_to_uci(m).as_deref(), Some(*uci));
                }
                json!({ "uci": uci, "value": value })
            })
            .collect();

    let mut head = Map::new();
    head.insert(
        "about".into(),
        Value::from(
            "Scacelith realtime protocol v1 golden vectors, generated by protogen from \
             dedicated-server/protocol/scacelith-v1.json (do not edit). valid[]: encoding `fields` gives `hex`, \
             decoding `hex` gives `fields`. malformed[]: every receiver in direction `dir` refuses `hex`; `reason` \
             is the Rust codec's reason. lenient[]: a lenient receiver (a client for s2c, a server reading a Hello \
             for c2s) decodes `hex` as `fields` (null: ignores it), a strict one refuses it with `strictReason`. \
             fnv1a32[]: posHash function. moves[]: move packing (null: not a move).",
        ),
    );
    head.insert("protocol".into(), Value::from(schema.protocol));
    head.insert("minor".into(), Value::from(schema.minor));
    head.insert("fingerprint".into(), Value::from(schema.fingerprint));
    head.insert("fingerprintHex".into(), Value::from(format!("{:#010x}", schema.fingerprint)));
    head.insert("subprotocol".into(), Value::from(schema.subprotocol.clone()));
    render(
        &head,
        &[
            ("valid", &b.valid),
            ("malformed", &b.malformed),
            ("lenient", &b.lenient),
            ("fnv1a32", &fnv),
            ("moves", &moves),
        ],
    )
}

/// Header keys one per line, then each list with one entry per line (stable and diff-friendly).
fn render(head: &Map<String, Value>, lists: &[(&str, &[Value])]) -> String {
    let mut out = String::from("{\n");
    for (k, v) in head {
        out.push_str(&format!("  {}: {},\n", Value::from(k.as_str()), v));
    }
    for (i, (name, items)) in lists.iter().enumerate() {
        out.push_str(&format!("  \"{name}\": [\n"));
        let lines: Vec<String> = items.iter().map(|v| format!("    {v}")).collect();
        out.push_str(&lines.join(",\n"));
        out.push_str(if i + 1 == lists.len() { "\n  ]\n" } else { "\n  ],\n" });
    }
    out.push_str("}\n");
    out
}

type EnumOf<'e> = dyn Fn(&str, &str) -> u8 + 'e;

fn edge_cases(b: &mut Builder<'_>, e: &EnumOf<'_>) {
    let schema = b.schema;
    let t = |key: &str| typical(schema, key);
    let hello = t("Hello");
    b.ok(
        "Hello",
        with(&hello, json!({ "client": "c".repeat(48), "token": format!("sct_{}", "T".repeat(156)) })),
        "client and token at their maximum length (48 and 160 bytes)",
    );
    b.ok(
        "Hello",
        with(&hello, json!({ "client": "", "token": "sct_0123456789ab" })),
        "empty client, token at its minimum length (16 bytes)",
    );
    b.ok("Hello", with(&hello, json!({ "seq": U32, "proto": 0xffff, "minor": 0xffff, "caps": MAX53, "client": "♞ Scacelith 🐴 Łódź" })),
        "u32 and u16 maxima, caps 2^53 - 1, 3- and 4-byte UTF-8");
    b.ok("C_Ping", json!({ "seq": 0, "nonce": 0 }), "zeros");
    b.ok("C_Pong", json!({ "seq": U32, "nonce": U32 }), "u32 maxima");
    b.ok(
        "QueueJoin",
        json!({ "seq": 2, "category": "1+0", "rated": false }),
        "category at its minimum length (3 bytes), casual",
    );
    b.ok(
        "QueueJoin",
        json!({ "seq": 2, "category": "180+180", "rated": true }),
        "category at its maximum length (7 bytes)",
    );
    b.ok("ChallengeCreate", json!({ "seq": 4, "target": "", "baseSec": 15, "incSec": 0, "rated": false, "color": e("ColorPref", "Random") }),
        "private game (empty target), baseSec at its minimum");
    b.ok("ChallengeCreate", json!({ "seq": 4, "target": "مُحَمَّد", "baseSec": 10_800, "incSec": 180, "rated": false, "color": e("ColorPref", "Black") }),
        "Arabic name with diacritics (16 bytes), baseSec and incSec at their maximum");
    b.ok("ChallengeCreate", json!({ "seq": 4, "target": "ユキユキユキユキ", "baseSec": 180, "incSec": 2, "rated": true, "color": e("ColorPref", "White") }),
        "target at its maximum length in 3-byte characters (24 bytes)");
    b.ok("ChallengeJoinCode", json!({ "seq": 4, "code": "AB12" }), "code at its minimum length (4)");
    b.ok("ChallengeJoinCode", json!({ "seq": 4, "code": "ABCD-EFGH-JK" }), "code at its maximum length (12)");
    b.ok("Move", json!({ "seq": 99, "game": MAX53, "ply": 1199, "move": 0x7fff, "posHash": U32, "thinkMs": U32, "drawOffer": true }),
        "game = 2^53 - 1, ply and move at their maximum, drawOffer");
    b.ok("Move", json!({ "seq": 13, "game": 1u64 << 32, "ply": 57, "move": mv("e7e8q"), "posHash": 0, "thinkMs": 0, "drawOffer": false }),
        "game = 2^32, promotion e7e8=Q");
    b.ok("Move", json!({ "seq": 14, "game": U32, "ply": 8, "move": mv("e1g1"), "posHash": 0x1234_5678, "thinkMs": 812, "drawOffer": false }),
        "game = 2^32 - 1, castling e1g1");
    b.ok("Resign", json!({ "seq": 1, "game": 1 }), "smallest game id");
    b.ok("Rematch", json!({ "seq": 31, "game": MAX53 - 1, "accept": false }), "decline, game = 2^53 - 2");
    b.ok("C_Gesture", json!({ "seq": 32, "game": GAME, "ply": 1199, "touch": 64, "aim": 64, "placed": 0x7fff, "flags": 7, "yaw": 3142, "pitch": 1571, "lean": 100 }),
        "every bounded field at its maximum");
    b.ok("C_Gesture", json!({ "seq": 33, "game": GAME, "ply": 0, "touch": 0, "aim": 0, "placed": mv("g1f3"), "flags": 4, "yaw": -3142, "pitch": -1571, "lean": 0 }),
        "signed fields at their minimum, look on the side table");
    let welcome = t("Welcome");
    b.ok("Welcome", with(&welcome, json!({
        "serverTime": 0.0, "userId": U32, "username": "ユキユキユキユキ", "serverName": format!("{}x", "♜".repeat(21)),
        "heartbeatMs": 0, "clientPingMs": U32, "maxMsgPerSec": 0xffff, "msgBurst": 0xffff, "activeGame": MAX53,
        "gestureRate": 60, "gestureBurst": 120, "gestureIdleMs": 0xffff,
    })), "username and serverName at their maximum (24 and 64 bytes), activeGame = 2^53 - 1, serverTime 0, gestureIdleMs above the range a client keeps");
    b.ok("Welcome", with(&welcome, json!({
        "serverTime": -123_456.789, "userId": 1, "username": "مُحَمَّد", "serverName": "", "clientPingMs": 0, "activeGame": GAME,
        "gestureRate": 0, "gestureBurst": 0, "gestureIdleMs": 0,
    })), "negative f64, Arabic username, empty serverName, a game to resume, no gesture relay");
    b.ok(
        "Welcome",
        with(&welcome, json!({ "minor": 0xffff, "caps": MAX53 })),
        "a later minor with capability bits this codec does not know (a client ignores both)",
    );
    b.ok(
        "Error",
        json!({ "ref": 0, "code": e("ErrorCode", "ProtocolViolation"), "fatal": true, "game": 0 }),
        "fatal, no request, no game",
    );
    b.ok(
        "Error",
        json!({ "ref": 1, "code": e("ErrorCode", "Malformed"), "fatal": true, "game": 0 }),
        "smallest ErrorCode",
    );
    b.ok(
        "Error",
        json!({ "ref": U32, "code": e("ErrorCode", "CheatDetected"), "fatal": false, "game": MAX53 }),
        "largest ErrorCode",
    );
    b.ok("S_Ping", json!({ "nonce": U32, "serverTime": f64::MAX }), "largest finite f64");
    b.ok("S_Pong", json!({ "nonce": 1, "serverTime": 0.1 }), "f64 0.1 (inexact in binary)");
    b.ok("Notice", json!({ "code": e("NoticeCode", "Banned"), "arg": T0 + 86_400_000.0 }), "ban end time");
    b.ok(
        "Notice",
        json!({ "code": e("NoticeCode", "MatchmakingCooldown"), "arg": -1e-300 }),
        "tiny negative f64",
    );
    b.ok(
        "Notice",
        json!({ "code": e("NoticeCode", "RatingRestored"), "arg": 0.0 }),
        "largest NoticeCode, no argument",
    );
    b.ok("QueueStatus", json!({ "category": "", "rated": false, "state": e("QueueState", "Left"), "waitMs": 0, "window": 0, "queued": 0 }),
        "left, empty category");
    b.ok("QueueStatus", json!({ "category": "180+180", "rated": true, "state": e("QueueState", "Matched"), "waitMs": U32, "window": 0xffff, "queued": U32 }),
        "maxima");
    b.ok("ChallengeReceived", json!({ "id": U32, "from": player(1, "Łukasz", 0, true), "baseSec": 10_800, "incSec": 180, "rated": false, "yourColor": e("ColorPref", "Random"), "expiresMs": 0 }),
        "Polish name, baseSec and incSec at their maximum");
    b.ok("ChallengeReceived", json!({ "id": 1, "from": player(3, "x", 65_535, false), "baseSec": 15, "incSec": 0, "rated": true, "yourColor": e("ColorPref", "White"), "expiresMs": 5000 }),
        "one-byte name (minimum), rating 65535, baseSec at its minimum");
    b.ok("ChallengeStatus", json!({ "id": 5, "state": e("ChallengeState", "Unavailable"), "target": "ABCDEFGHIJKLMNOPQRSTUVWX", "code": "", "baseSec": 15, "incSec": 0, "rated": true }),
        "target at its maximum length (24), last ChallengeState");
    b.ok("GameSnapshot", snapshot(json!({
        "gseq": 0, "moves": [], "running": 2, "whiteMs": 180_000, "blackMs": 180_000, "firstMoveMs": 30_000, "you": 1, "startedAt": T0,
    })), "game start: empty move list, clocks not running, first-move timer");
    b.ok("GameSnapshot", snapshot(json!({
        "category": "custom", "rated": false, "baseMs": 0, "incMs": 0, "gseq": U32, "drawOffer": 1, "rematch": 0,
        "status": e("GameStatus", "Draw"), "reason": e("EndReason", "BothDisconnected"), "running": 2,
        "whiteConnected": false, "blackConnected": false, "white": player(U32, "مُحَمَّد", 0, false),
        "black": player(0, "ユキユキユキユキ", 65_535, true), "you": 1,
    })), "finished game, largest EndReason, multi-byte names, custom category");
    b.ok(
        "GameSnapshot",
        snapshot(json!({ "gseq": 80, "moves": move_list(80, 80), "whiteMs": 54_321, "blackMs": 43_210 })),
        "80 moves (the benchmark message)",
    );
    b.ok("GameSnapshot", snapshot(json!({
        "gseq": 1200, "moves": move_list(1200, 1200), "status": e("GameStatus", "Draw"), "reason": e("EndReason", "ServerAborted"), "running": 2,
    })), "move list at its maximum (1200 plies)");
    b.ok("MoveMade", json!({
        "game": MAX53, "gseq": U32, "ply": 1199, "move": 0x7fff, "flags": 0xff, "spentMs": U32, "whiteMs": U32, "blackMs": 0,
        "serverTime": 1e300, "drawOffer": true, "firstMoveMs": U32,
    }), "maxima, f64 1e300");
    b.ok("MoveMade", json!({
        "game": GAME, "gseq": 1, "ply": 0, "move": mv("e2e4"), "flags": 16, "spentMs": 0, "whiteMs": 180_000, "blackMs": 180_000,
        "serverTime": T0, "drawOffer": false, "firstMoveMs": 30_000,
    }), "first move: no clock charge, first-move timer for Black");
    b.ok(
        "MoveRejected",
        json!({ "game": GAME, "ply": 1199, "move": 0x7fff, "code": e("ErrorCode", "FlagFell") }),
        "FlagFell, maxima",
    );
    b.ok("GameEvent", json!({ "game": GAME, "gseq": 3, "kind": e("GameEventKind", "RematchDeclined"), "color": 2, "arg": U32 }),
        "largest GameEventKind, color None");
    b.ok("GameEnd", json!({
        "game": GAME, "gseq": 2, "status": e("GameStatus", "Aborted"), "reason": e("EndReason", "NoShow"), "whiteMs": 180_000,
        "blackMs": 180_000, "serverTime": T0,
    }), "aborted (no-show)");
    b.ok(
        "RatingUpdate",
        json!({
            "game": MAX53, "category": "180+180",
            "white": { "before": 0, "after": 65_535, "games": U32, "provisional": true },
            "black": { "before": 65_535, "after": 0, "games": 0, "provisional": false },
        }),
        "extremes",
    );
}

fn malformed(b: &mut Builder<'_>) {
    let schema = b.schema;
    let t = |key: &str| typical(schema, key);
    let (c2s, s2c) = (Dir::C2s, Dir::S2c);

    b.bad(vec![], c2s, "empty", "empty message (server side)");
    b.bad(vec![], s2c, "empty", "empty message (client side)");
    b.bad(vec![0x00], c2s, "unknown type", "type 0x00");
    b.bad(vec![0x04, 1, 0, 0, 0], c2s, "unknown type", "unassigned client type 0x04");
    b.bad(vec![0x70, 1, 0, 0, 0], c2s, "unknown type", "experimental client type 0x70");
    b.bad(vec![0x7f, 1, 0, 0, 0], c2s, "unknown type", "unassigned client type 0x7F");
    b.bad(
        b.enc("Welcome", &t("Welcome")),
        c2s,
        "wrong direction",
        "server message (Welcome) received by the server",
    );
    b.bad(b.enc("S_Ping", &t("S_Ping")), c2s, "wrong direction", "server Ping received by the server");

    // Truncation and trailing bytes, for every message.
    for m in &schema.messages {
        let bytes = b.enc(&m.key, &t(&m.key));
        b.bad(bytes[..1].to_vec(), m.dir, "truncated", &format!("{}: type byte only", m.key));
        b.bad(
            bytes[..bytes.len() - 1].to_vec(),
            m.dir,
            "truncated",
            &format!("{}: last byte missing", m.key),
        );
        if m.dir == c2s {
            let mut longer = bytes.clone();
            longer.push(0);
            let note = if m.id == 1 { "Hello of minor 0 with one extra byte" } else { "one extra byte" };
            b.bad(longer, c2s, "trailing bytes", &format!("{}: {note}", m.key));
        }
    }
    b.bad(
        b.enc("QueueLeave", &t("QueueLeave"))[..3].to_vec(),
        c2s,
        "truncated",
        "QueueLeave with a 2-byte seq",
    );
    let hello = b.enc("Hello", &t("Hello"));
    b.bad(hello[..hello.len() - 10].to_vec(), c2s, "truncated", "Hello cut inside the token");
    b.bad(
        hello[..b.offset("Hello", &t("Hello"), "token")].to_vec(),
        c2s,
        "truncated",
        "Hello without the token length byte",
    );
    b.bad(hello[..12].to_vec(), c2s, "truncated", "Hello cut inside the frozen prefix (caps)");
    let mm = b.enc("MoveMade", &t("MoveMade"));
    b.bad(
        mm[..b.offset("MoveMade", &t("MoveMade"), "serverTime") + 5].to_vec(),
        s2c,
        "truncated",
        "MoveMade cut inside serverTime",
    );
    let snap_fields = t("GameSnapshot");
    let snap = b.enc("GameSnapshot", &snap_fields);
    b.bad(
        snap[..b.offset("GameSnapshot", &snap_fields, "moves.4")].to_vec(),
        s2c,
        "truncated",
        "GameSnapshot cut inside the move list",
    );
    b.bad(snap[..snap.len() - 3].to_vec(), s2c, "truncated", "GameSnapshot cut inside startedAt");
    b.bad(
        snap[..b.offset("GameSnapshot", &snap_fields, "white.name") + 3].to_vec(),
        s2c,
        "truncated",
        "GameSnapshot cut inside white.name",
    );

    // Closed enums (open enums are in lenient[]).
    b.bad(
        b.patch("ChallengeCreate", &t("ChallengeCreate"), "color", &[3]),
        c2s,
        "color not in ColorPref",
        "ChallengeCreate.color = 3",
    );
    b.bad(
        b.patch("ChallengeReceived", &t("ChallengeReceived"), "yourColor", &[0xff]),
        s2c,
        "yourColor not in ColorPref",
        "ChallengeReceived.yourColor = 255",
    );
    b.bad(
        b.patch("GameSnapshot", &snap_fields, "you", &[3]),
        s2c,
        "you not in Color",
        "GameSnapshot.you = 3",
    );
    b.bad(
        b.patch("GameSnapshot", &snap_fields, "running", &[3]),
        s2c,
        "running not in Color",
        "GameSnapshot.running = 3",
    );
    b.bad(
        b.patch("GameEvent", &t("GameEvent"), "color", &[0x80]),
        s2c,
        "color not in Color",
        "GameEvent.color = 128",
    );
    b.bad(
        b.patch("GameEnd", &t("GameEnd"), "status", &[5]),
        s2c,
        "status not in GameStatus",
        "GameEnd.status = 5",
    );

    // Bools.
    b.bad(b.patch("Move", &t("Move"), "drawOffer", &[2]), c2s, "drawOffer not a bool", "Move.drawOffer = 2");
    b.bad(
        b.patch("QueueJoin", &t("QueueJoin"), "rated", &[2]),
        c2s,
        "rated not a bool",
        "QueueJoin.rated = 2",
    );
    b.bad(
        b.patch("ChallengeReceived", &t("ChallengeReceived"), "from.provisional", &[2]),
        s2c,
        "from.provisional not a bool",
        "ChallengeReceived.from.provisional = 2",
    );
    b.bad(
        b.patch("GameSnapshot", &snap_fields, "whiteConnected", &[0xff]),
        s2c,
        "whiteConnected not a bool",
        "GameSnapshot.whiteConnected = 255",
    );
    b.bad(b.patch("Error", &t("Error"), "fatal", &[2]), s2c, "fatal not a bool", "Error.fatal = 2");

    // Strings: UTF-8, NUL, length bounds.
    let hello_fields = t("Hello");
    for (raw, note) in [
        (&[0x41, 0xc3, 0x28][..], "invalid continuation byte (C3 28)"),
        (&[0xc0, 0xaf], "overlong encoding of \"/\" (C0 AF)"),
        (&[0xe0, 0x80, 0xaf], "overlong 3-byte encoding (E0 80 AF)"),
        (&[0xed, 0xa0, 0x80], "encoded UTF-16 surrogate U+D800 (ED A0 80)"),
        (&[0x61, 0xe3, 0x81], "multi-byte sequence cut at the end of the string (E3 81)"),
        (&[0xf4, 0x90, 0x80, 0x80], "code point above U+10FFFF (F4 90 80 80)"),
        (&[0xff], "byte FF"),
        (&[0x80, 0x61], "lone continuation byte (80)"),
    ] {
        b.bad(
            b.str_bytes("Hello", &hello_fields, "client", raw),
            c2s,
            "client not UTF-8",
            &format!("Hello.client: {note}"),
        );
    }
    b.bad(
        b.str_bytes("ChallengeReceived", &t("ChallengeReceived"), "from.name", &[0xe3, 0x82]),
        s2c,
        "from.name not UTF-8",
        "PlayerInfo.name with a cut sequence",
    );
    b.bad(
        b.str_bytes("Hello", &hello_fields, "client", b"ab\0c"),
        c2s,
        "client contains NUL",
        "NUL inside a string",
    );
    b.bad(
        b.str_bytes("Welcome", &t("Welcome"), "username", b"\0"),
        s2c,
        "username contains NUL",
        "username = NUL",
    );
    let splices: [(&str, &str, Dir, Vec<u8>, &str); 12] = [
        ("Hello", "client", c2s, vec![b'c'; 49], "Hello.client of 49 bytes (max 48)"),
        ("Hello", "token", c2s, vec![b't'; 161], "Hello.token of 161 bytes (max 160)"),
        ("Hello", "token", c2s, vec![b't'; 15], "Hello.token of 15 bytes (min 16)"),
        ("QueueJoin", "category", c2s, b"3+".to_vec(), "QueueJoin.category of 2 bytes (min 3)"),
        ("QueueJoin", "category", c2s, b"180+1800".to_vec(), "QueueJoin.category of 8 bytes (max 7)"),
        ("ChallengeJoinCode", "code", c2s, b"ABC".to_vec(), "ChallengeJoinCode.code of 3 bytes (min 4)"),
        (
            "Welcome",
            "username",
            s2c,
            "ユキユキユキユキa".as_bytes().to_vec(),
            "Welcome.username of 25 bytes (max 24)",
        ),
        ("Welcome", "username", s2c, vec![], "empty Welcome.username (min 1)"),
        ("GameSnapshot", "black.name", s2c, vec![], "empty PlayerInfo.name (min 1)"),
        ("GameSnapshot", "white.name", s2c, vec![b'w'; 25], "PlayerInfo.name of 25 bytes (max 24)"),
        ("GameSnapshot", "category", s2c, vec![], "empty GameSnapshot.category (min 1)"),
        ("RatingUpdate", "category", s2c, b"180+1800".to_vec(), "RatingUpdate.category of 8 bytes (max 7)"),
    ];
    for (key, path, dir, raw, note) in splices {
        let reason = format!("{path} bad length");
        b.bad(b.str_splice(key, &t(key), path, &raw), dir, &reason, note);
    }

    // A list one item over its maximum.
    {
        let f = snapshot(json!({ "moves": move_list(1200, 1201) }));
        let bytes = b.enc("GameSnapshot", &f);
        let at = b.offset("GameSnapshot", &f, "moves");
        let end = at + 2 + 1200 * 10;
        let mut over = bytes[..end].to_vec();
        over.extend_from_slice(&mv("a2a3").to_le_bytes());
        over.extend_from_slice(&[0; 8]);
        over.extend_from_slice(&bytes[end..]);
        over[at..at + 2].copy_from_slice(&1201u16.to_le_bytes());
        b.bad(over, s2c, "moves too long", "GameSnapshot with 1201 moves (max 1200)");
        let mut short = bytes[..at + 2].to_vec();
        short.extend_from_slice(&[0; 10]);
        b.bad(short, s2c, "truncated", "GameSnapshot announcing 1200 moves with one present");
    }

    // id53, integer bounds.
    let high = |v: u32| v.to_le_bytes();
    b.bad(
        b.patch("Move", &t("Move"), "game", &[&GAME.to_le_bytes()[..4], &high(0x0020_0000)[..]].concat()),
        c2s,
        "game above 2^53",
        "Move.game = 2^53 + low bits",
    );
    b.bad(
        b.patch("Move", &t("Move"), "game", &[0xff; 8]),
        c2s,
        "game above 2^53",
        "Move.game with every bit set",
    );
    b.bad(
        b.patch("Welcome", &t("Welcome"), "activeGame", &(1u64 << 63).to_le_bytes()),
        s2c,
        "activeGame above 2^53",
        "Welcome.activeGame = 2^63",
    );
    b.bad(
        b.patch("Error", &t("Error"), "game", &(1u64 << 53).to_le_bytes()),
        s2c,
        "game above 2^53",
        "Error.game = 2^53",
    );
    let u16le = |v: u16| v.to_le_bytes();
    let i32le = |v: i32| v.to_le_bytes();
    // (message, field, direction, bytes written at the field, defect, note)
    type Case = (&'static str, &'static str, Dir, Vec<u8>, &'static str, &'static str);
    let bounds: Vec<Case> = vec![
        ("Move", "move", c2s, u16le(0x8000 | mv("e2e4")).to_vec(), "above max", "Move.move with bit 15 set"),
        ("Move", "ply", c2s, u16le(1200).to_vec(), "above max", "Move.ply = 1200 (max 1199)"),
        (
            "ChallengeCreate",
            "baseSec",
            c2s,
            u16le(14).to_vec(),
            "below min",
            "ChallengeCreate.baseSec = 14 (min 15)",
        ),
        (
            "ChallengeCreate",
            "baseSec",
            c2s,
            u16le(10_801).to_vec(),
            "above max",
            "ChallengeCreate.baseSec = 10801 (max 10800)",
        ),
        ("ChallengeCreate", "incSec", c2s, vec![181], "above max", "ChallengeCreate.incSec = 181 (max 180)"),
        ("C_Gesture", "yaw", c2s, i32le(-3143).to_vec(), "below min", "Gesture.yaw = -3143 (min -3142)"),
        ("C_Gesture", "yaw", c2s, i32le(3143).to_vec(), "above max", "Gesture.yaw = 3143 (max 3142)"),
        ("C_Gesture", "yaw", c2s, i32le(i32::MIN).to_vec(), "below min", "Gesture.yaw = INT32_MIN"),
        ("C_Gesture", "pitch", c2s, i32le(-1572).to_vec(), "below min", "Gesture.pitch = -1572 (min -1571)"),
        ("C_Gesture", "pitch", c2s, i32le(1572).to_vec(), "above max", "Gesture.pitch = 1572 (max 1571)"),
        ("C_Gesture", "touch", c2s, vec![65], "above max", "Gesture.touch = 65 (max 64)"),
        ("C_Gesture", "flags", c2s, vec![8], "above max", "Gesture.flags = 8 (max 7)"),
        ("C_Gesture", "lean", c2s, vec![101], "above max", "Gesture.lean = 101 (max 100)"),
        ("C_Gesture", "ply", c2s, u16le(1200).to_vec(), "above max", "Gesture.ply = 1200 (max 1199)"),
        (
            "C_Gesture",
            "placed",
            c2s,
            u16le(0x8000).to_vec(),
            "above max",
            "Gesture.placed = 0x8000 (max 0x7FFF)",
        ),
        (
            "S_Gesture",
            "yaw",
            s2c,
            i32le(-3143).to_vec(),
            "below min",
            "server Gesture.yaw = -3143 (min -3142)",
        ),
        ("S_Gesture", "yaw", s2c, i32le(3143).to_vec(), "above max", "server Gesture.yaw = 3143 (max 3142)"),
        (
            "S_Gesture",
            "pitch",
            s2c,
            i32le(i32::MIN).to_vec(),
            "below min",
            "server Gesture.pitch = INT32_MIN",
        ),
        (
            "S_Gesture",
            "pitch",
            s2c,
            i32le(1572).to_vec(),
            "above max",
            "server Gesture.pitch = 1572 (max 1571)",
        ),
        ("S_Gesture", "aim", s2c, vec![65], "above max", "server Gesture.aim = 65 (max 64)"),
        ("Welcome", "gestureRate", s2c, u16le(61).to_vec(), "above max", "Welcome.gestureRate = 61 (max 60)"),
        (
            "Welcome",
            "gestureBurst",
            s2c,
            u16le(121).to_vec(),
            "above max",
            "Welcome.gestureBurst = 121 (max 120)",
        ),
        ("MoveMade", "ply", s2c, u16le(1200).to_vec(), "above max", "MoveMade.ply = 1200 (max 1199)"),
        ("MoveMade", "move", s2c, u16le(0xffff).to_vec(), "above max", "MoveMade.move with bit 15 set"),
        ("MoveRejected", "ply", s2c, u16le(1200).to_vec(), "above max", "MoveRejected.ply = 1200 (max 1199)"),
        (
            "ChallengeReceived",
            "incSec",
            s2c,
            vec![181],
            "above max",
            "ChallengeReceived.incSec = 181 (max 180)",
        ),
        (
            "ChallengeStatus",
            "baseSec",
            s2c,
            u16le(14).to_vec(),
            "below min",
            "ChallengeStatus.baseSec = 14 (min 15)",
        ),
    ];
    for (key, path, dir, raw, defect, note) in bounds {
        b.bad(b.patch(key, &t(key), path, &raw), dir, &format!("{path} {defect}"), note);
    }
    b.bad(
        b.patch("GameSnapshot", &snap_fields, "moves.3.move", &u16le(0xffff)),
        s2c,
        "moves.move above max",
        "MoveRec.move with bit 15 set",
    );

    // Non-finite f64.
    let f64bits = |bits: u64| bits.to_le_bytes();
    for (key, path, bits, note) in [
        ("Welcome", "serverTime", 0x7ff8_0000_0000_0000u64, "Welcome.serverTime = NaN"),
        ("S_Ping", "serverTime", 0x7ff0_0000_0000_0000, "Ping.serverTime = +Infinity"),
        ("Notice", "arg", 0xfff0_0000_0000_0000, "Notice.arg = -Infinity"),
        ("GameEnd", "serverTime", 0x7ff0_0000_0000_0001, "GameEnd.serverTime = signalling NaN"),
        ("GameSnapshot", "startedAt", 0xfff8_0000_0000_0000, "GameSnapshot.startedAt = negative NaN"),
    ] {
        b.bad(b.patch(key, &t(key), path, &f64bits(bits)), s2c, &format!("{path} not finite"), note);
    }
}

fn lenient(b: &mut Builder<'_>, e: &EnumOf<'_>) {
    let schema = b.schema;
    let t = |key: &str| typical(schema, key);
    let s2c = Dir::S2c;

    for (bytes, note) in [
        (vec![0x00], "type 0x00"),
        (vec![0xa7, 0], "unassigned server type 0xA7"),
        (vec![0xc0, 1, 2, 3], "server type 0xC0 of a later minor"),
        (vec![0xf0], "experimental server type 0xF0"),
        (vec![0xff], "type 0xFF"),
    ] {
        b.lenient(bytes, s2c, note);
    }
    b.lenient(b.enc("Move", &t("Move")), s2c, "client message (Move) received by a client");
    b.lenient(b.enc("C_Pong", &t("C_Pong")), s2c, "client Pong received by a client");

    let mut ack = b.enc("Ack", &t("Ack"));
    ack.extend_from_slice(&[0xff; 3]);
    b.lenient(ack, s2c, "Ack with three bytes of a later minor");
    let mut snap = b.enc("GameSnapshot", &t("GameSnapshot"));
    snap.push(0);
    b.lenient(snap, s2c, "GameSnapshot with one byte of a later minor");
    let mut welcome = b.enc("Welcome", &with(&t("Welcome"), json!({ "minor": 1, "caps": 5 })));
    welcome.extend_from_slice(&[0x2a, 0, 0, 0, 3, b'a', b'b', b'c']);
    b.lenient(welcome, s2c, "Welcome of a minor 1 server with two appended fields");

    let unknown: [(&str, &str, u8, &str); 15] = [
        ("Error", "code", 0, "Error.code = 0"),
        ("Error", "code", 12, "Error.code = 12 (after EmailUnverified)"),
        ("Error", "code", e("ErrorCode", "CheatDetected") + 1, "Error.code = 243 (SlowConsumer, retired)"),
        ("Error", "code", 244, "Error.code = 244"),
        ("MoveRejected", "code", 99, "MoveRejected.code = 99"),
        ("GameSnapshot", "reason", 14, "GameSnapshot.reason = 14 (gap before Abandonment)"),
        ("GameSnapshot", "reason", 27, "GameSnapshot.reason = 27 (after BothDisconnected)"),
        ("GameEnd", "reason", 255, "GameEnd.reason = 255"),
        ("GameEvent", "kind", 0, "GameEvent.kind = 0"),
        ("GameEvent", "kind", 7, "GameEvent.kind = 7 (AbortAvailable, retired)"),
        ("GameEvent", "kind", 8, "GameEvent.kind = 8"),
        ("Notice", "code", 6, "Notice.code = 6 (Motd, retired)"),
        ("Notice", "code", 255, "Notice.code = 255"),
        ("QueueStatus", "state", 3, "QueueStatus.state = 3"),
        ("ChallengeStatus", "state", 6, "ChallengeStatus.state = 6"),
    ];
    for (key, path, value, note) in unknown {
        let fields = t(key);
        let bytes = b.patch(key, &fields, path, &[value]);
        b.lenient(bytes, s2c, &format!("{note}: a value of a later minor"));
    }

    let mut hello = b.enc("Hello", &with(&t("Hello"), json!({ "minor": 1, "caps": 3 })));
    hello.extend_from_slice(&[7, 0, 0, 0]);
    b.lenient(
        hello,
        Dir::C2s,
        "Hello of a minor 1 client with an appended field (the server reads its own fields)",
    );
}
