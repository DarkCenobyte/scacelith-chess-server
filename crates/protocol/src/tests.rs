//! Conformance tests of the generated codec: the golden vectors, a differential comparison with
//! protogen's interpreted codec on random and mutated inputs, and the hand-written helpers.

use serde_json::Value;

use crate::codegen::interp::{self, Decoded, Rng};
use crate::codegen::model::{Dir, Schema};
use crate::gen_json::{client_from_json, client_to_json, server_from_json, server_to_json};
use crate::*;

fn schema() -> Schema {
    crate::codegen::load(&crate::codegen::default_root()).expect("the schema loads")
}

fn vectors() -> Value {
    let path = crate::codegen::default_root().join("test/fixtures/protocol-vectors.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("the vectors parse")
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex")).collect()
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or_else(|| panic!("vectors: no {key}[]"))
}

/// What the generated codec makes of `bytes` received in direction `dir`, in the interpreter's
/// terms (`strict`: the server's decoding for c2s, `decode_exact` for s2c).
fn generated(bytes: &[u8], dir: Dir, strict: bool) -> Result<Decoded, String> {
    let message = |(key, fields): (&str, Value)| Decoded::Message { key: key.to_owned(), fields };
    match (dir, strict) {
        (Dir::C2s, _) => {
            ClientMsg::decode(bytes).map(|m| message(client_to_json(&m))).map_err(|e| e.to_string())
        }
        (Dir::S2c, true) => {
            ServerMsg::decode_exact(bytes).map(|m| message(server_to_json(&m))).map_err(|e| e.to_string())
        }
        (Dir::S2c, false) => match ServerMsg::decode(bytes) {
            Ok(Some(m)) => Ok(message(server_to_json(&m))),
            Ok(None) => Ok(Decoded::Ignored),
            Err(e) => Err(e.to_string()),
        },
    }
}

fn encode_json(key: &str, fields: &Value) -> Vec<u8> {
    let bytes = match client_from_json(key, fields) {
        Some(m) => m.to_bytes(),
        None => {
            server_from_json(key, fields).unwrap_or_else(|| panic!("{key}: fields do not convert")).to_bytes()
        }
    };
    bytes.unwrap_or_else(|e| panic!("{key}: {e}")).to_vec()
}

#[test]
fn header_matches_the_constants() {
    let v = vectors();
    assert_eq!(v["protocol"], PROTOCOL_VERSION);
    assert_eq!(v["minor"], MINOR);
    assert_eq!(v["fingerprint"], FINGERPRINT);
    assert_eq!(v["subprotocol"], SUBPROTOCOL);
    assert_eq!(schema().fingerprint, FINGERPRINT);
}

#[test]
fn valid_vectors_round_trip() {
    let v = vectors();
    let valid = list(&v, "valid");
    assert!(valid.len() >= MsgType::ALL.len() * 2);
    for t in MsgType::ALL {
        assert!(valid.iter().any(|x| x["name"] == t.name()), "no vector for {}", t.name());
    }
    for x in valid {
        let (name, note) = (x["name"].as_str().unwrap(), &x["note"]);
        let bytes = unhex(x["hex"].as_str().unwrap());
        let dir = if x["dir"] == "c2s" { Dir::C2s } else { Dir::S2c };
        let want = Decoded::Message { key: name.to_owned(), fields: x["fields"].clone() };
        for strict in [true, false] {
            assert_eq!(generated(&bytes, dir, strict).as_ref(), Ok(&want), "{name} ({note})");
        }
        assert_eq!(encode_json(name, &x["fields"]), bytes, "{name} ({note}): encoding");
        assert_eq!(peek_type(&bytes).map(MsgType::name), Some(name));
        assert_eq!(x["type"], bytes[0]);
        let len = match dir {
            Dir::C2s => ClientMsg::decode(&bytes).unwrap().encoded_len(),
            Dir::S2c => ServerMsg::decode_exact(&bytes).unwrap().encoded_len(),
        };
        assert_eq!(len, bytes.len(), "{name} ({note}): encoded_len");
    }
}

#[test]
fn malformed_vectors_are_refused_with_their_reason() {
    let v = vectors();
    let malformed = list(&v, "malformed");
    assert!(malformed.len() >= 150);
    for x in malformed {
        let (reason, note) = (x["reason"].as_str().unwrap(), &x["note"]);
        let bytes = unhex(x["hex"].as_str().unwrap());
        let dir = if x["dir"] == "c2s" { Dir::C2s } else { Dir::S2c };
        for strict in [true, false] {
            assert_eq!(generated(&bytes, dir, strict), Err(reason.to_owned()), "{note}");
        }
        if dir == Dir::C2s && bytes.first() == Some(&0x01) {
            assert!(decode_hello(&bytes).is_err(), "{note}: accepted by decode_hello");
        }
    }
}

#[test]
fn lenient_vectors() {
    let v = vectors();
    for x in list(&v, "lenient") {
        let note = &x["note"];
        let bytes = unhex(x["hex"].as_str().unwrap());
        let strict_reason = x["strictReason"].as_str().unwrap().to_owned();
        if x["dir"] == "c2s" {
            assert_eq!(
                ClientMsg::decode(&bytes).map(|_| ()).map_err(|e| e.to_string()),
                Err(strict_reason),
                "{note}"
            );
            let hello = decode_hello(&bytes).unwrap_or_else(|e| panic!("{note}: {e}"));
            assert_eq!(client_to_json(&ClientMsg::Hello(hello)).1, x["fields"], "{note}");
            continue;
        }
        assert_eq!(generated(&bytes, Dir::S2c, true), Err(strict_reason), "{note}");
        let want = match &x["fields"] {
            Value::Null => Decoded::Ignored,
            fields => {
                Decoded::Message { key: x["name"].as_str().unwrap().to_owned(), fields: fields.clone() }
            }
        };
        assert_eq!(generated(&bytes, Dir::S2c, false), Ok(want), "{note}");
    }
}

#[test]
fn position_and_move_vectors() {
    let v = vectors();
    for x in list(&v, "fnv1a32") {
        assert_eq!(fnv1a32(x["text"].as_str().unwrap().as_bytes()), x["hash"].as_u64().unwrap() as u32);
        assert_eq!(fen_digest(x["text"].as_str().unwrap()), x["hash"].as_u64().unwrap() as u32);
    }
    for x in list(&v, "moves") {
        let uci = x["uci"].as_str().unwrap();
        assert_eq!(uci_to_move(uci).map(u64::from), x["value"].as_u64(), "{uci}");
    }
}

/// Random valid messages: the generated codec encodes them as the interpreter does and decodes
/// them back.
#[test]
fn random_messages_match_the_interpreter() {
    let schema = schema();
    let mut rng = Rng::new(0x5ca6_e117);
    for m in &schema.messages {
        for _ in 0..200 {
            let fields = interp::random_fields(&schema, &m.fields, &mut rng);
            let bytes = interp::encode(&schema, m, &fields).unwrap();
            let Ok(Decoded::Message { fields, .. }) = interp::decode(&schema, &bytes, m.dir, true) else {
                panic!("{}: the interpreter refuses its own encoding", m.key);
            };
            assert_eq!(encode_json(&m.key, &fields), bytes, "{}", m.key);
            let want = Decoded::Message { key: m.key.clone(), fields };
            assert_eq!(generated(&bytes, m.dir, true), Ok(want.clone()), "{}", m.key);
            assert_eq!(generated(&bytes, m.dir, false), Ok(want), "{}", m.key);
        }
    }
}

/// Mutated and random frames never panic, and the generated codec accepts, ignores or refuses
/// them exactly as the interpreter does, with the same reason.
#[test]
fn mutations_match_the_interpreter() {
    let schema = schema();
    let v = vectors();
    let mut rng = Rng::new(42);
    let check = |bytes: &[u8]| {
        for dir in [Dir::C2s, Dir::S2c] {
            for strict in [true, false] {
                let want = interp::decode(&schema, bytes, dir, strict);
                assert_eq!(generated(bytes, dir, strict), want, "{bytes:02x?} ({dir:?}, strict {strict})");
            }
        }
        let hello =
            decode_hello(bytes).map(|h| client_to_json(&ClientMsg::Hello(h)).1).map_err(|e| e.to_string());
        assert_eq!(hello, interp::decode_hello(&schema, bytes), "{bytes:02x?} (Hello)");
        let _ = (peek_type(bytes), peek_seq(bytes), HelloPrefix::read(bytes));
    };
    let seeds: Vec<Vec<u8>> = list(&v, "valid").iter().map(|x| unhex(x["hex"].as_str().unwrap())).collect();
    for seed in &seeds {
        for _ in 0..60 {
            let mut b = seed.clone();
            match rng.below(5) {
                0 => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] ^= 1 << rng.below(8);
                }
                1 => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] = rng.next_u64() as u8;
                }
                2 => b.truncate(rng.below(b.len() as u64) as usize),
                3 => b.push(rng.next_u64() as u8),
                _ => {
                    let i = 1 + rng.below(b.len() as u64) as usize;
                    b.insert(i.min(b.len()), rng.next_u64() as u8);
                }
            }
            check(&b);
        }
    }
    for _ in 0..20_000 {
        let len = rng.below(64) as usize;
        let b: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        check(&b);
    }
}

#[test]
fn encoding_validates_and_writes_nothing_on_error() {
    let welcome = Welcome { username: "ok".into(), ..Welcome::default() };
    let mut out = vec![0xAA];
    let bad = Welcome { username: String::new(), ..welcome.clone() };
    assert_eq!(bad.encode(&mut out).unwrap_err().to_string(), "cannot encode Welcome.username: bad length");
    assert_eq!(out, [0xAA]);
    let bad = Welcome { server_time: f64::NAN, ..welcome.clone() };
    assert_eq!(bad.to_vec().unwrap_err().defect(), Defect::NotFinite);
    let bad = Welcome { active_game: 1 << 53, ..welcome.clone() };
    assert_eq!(bad.to_vec().unwrap_err().defect(), Defect::AboveId53);
    let bad = Welcome { gesture_rate: 61, ..welcome };
    assert_eq!(bad.to_vec().unwrap_err().defect(), Defect::AboveMax);
    let bad = Error { code: ErrorCode::Unknown(250), ..Error::default() };
    assert_eq!(bad.to_vec().unwrap_err().to_string(), "cannot encode Error.code: not in ErrorCode");
    let player = PlayerInfo { name: "x".into(), ..PlayerInfo::default() };
    let snapshot = GameSnapshot {
        category: "3+2".into(),
        white: PlayerInfo { name: "w".repeat(25), ..player.clone() },
        black: player.clone(),
        ..GameSnapshot::default()
    };
    let e = snapshot.to_vec().unwrap_err();
    assert_eq!((e.message(), e.field(), e.defect()), ("GameSnapshot", "white.name", Defect::BadLength));
    let snapshot = GameSnapshot {
        category: "3+2".into(),
        white: player.clone(),
        black: player,
        moves: vec![MoveRec::default(); MAX_PLIES + 1],
        ..GameSnapshot::default()
    };
    assert_eq!(snapshot.to_vec().unwrap_err().defect(), Defect::TooLong);
    let gesture = ClientGesture { yaw: -3143, ..ClientGesture::default() };
    assert_eq!(gesture.to_vec().unwrap_err().defect(), Defect::BelowMin);
}

#[test]
fn negative_zero_and_full_caps() {
    let a = ServerPing { nonce: 1, server_time: -0.0 }.to_vec().unwrap();
    let b = ServerPing { nonce: 1, server_time: 0.0 }.to_vec().unwrap();
    assert_eq!(a, b);
    let hello = Hello { seq: 1, proto: 1, caps: u64::MAX, token: "t".repeat(16), ..Hello::default() };
    let bytes = hello.to_vec().unwrap();
    assert_eq!(ClientMsg::decode(&bytes).unwrap(), ClientMsg::Hello(hello.clone()));
    assert_eq!(HelloPrefix::read(&bytes), Some(HelloPrefix { seq: 1, proto: 1, minor: 0, caps: u64::MAX }));
    let welcome = Welcome { caps: u64::MAX, minor: u16::MAX, username: "u".into(), ..Welcome::default() };
    assert_eq!(ServerMsg::decode(&welcome.to_vec().unwrap()), Ok(Some(ServerMsg::Welcome(welcome))));
}

#[test]
fn hello_of_any_minor() {
    let hello = Hello { seq: 1, proto: 1, minor: 0, token: "t".repeat(16), ..Hello::default() };
    let mut bytes = hello.to_vec().unwrap();
    assert_eq!(decode_hello(&bytes), Ok(hello.clone()));
    bytes.push(9);
    assert_eq!(decode_hello(&bytes).unwrap_err().defect(), Defect::TrailingBytes);
    let later = Hello { minor: MINOR + 1, ..hello };
    let mut bytes = later.to_vec().unwrap();
    bytes.extend_from_slice(&[1, 2, 3]);
    assert_eq!(decode_hello(&bytes), Ok(later));
    assert_eq!(HelloPrefix::read(&bytes[..HELLO_PREFIX_LEN]).map(|p| p.minor), Some(MINOR + 1));
    assert_eq!(HelloPrefix::read(&bytes[..HELLO_PREFIX_LEN - 1]), None);
    let mut proto9 = bytes[..HELLO_PREFIX_LEN].to_vec();
    proto9[5] = 9;
    assert_eq!(HelloPrefix::read(&proto9).map(|p| p.proto), Some(9));
    assert_eq!(HelloPrefix::read(&Resign::default().to_vec().unwrap()), None);
}

#[test]
fn peeking() {
    let frame = Move { seq: 0x0102_0304, ..Move::default() }.to_vec().unwrap();
    assert_eq!(peek_seq(&frame), Some(0x0102_0304));
    assert_eq!(peek_type(&frame), Some(MsgType::Move));
    assert_eq!(peek_seq(&[0x7f, 1, 0, 0, 0]), Some(1));
    assert_eq!(peek_seq(&[0x20, 1, 0, 0]), None);
    assert_eq!(peek_seq(&[0x84, 1, 0, 0, 0]), None);
    assert_eq!(peek_seq(&[0x00, 1, 0, 0, 0]), None);
    assert_eq!(peek_type(&[]), None);
    assert_eq!(peek_type(&[0x7f]), None);
}

#[test]
fn close_codes_follow_the_rule() {
    for code in ErrorCode::ALL {
        match close_code_for(code) {
            Some(close) => assert_eq!(error_code_for_close(close), Some(code)),
            None => assert!((100..=239).contains(&code.to_u8())),
        }
    }
    let pairs = [
        (ErrorCode::Malformed, close::MALFORMED),
        (ErrorCode::UnsupportedProtocol, close::UNSUPPORTED_PROTOCOL),
        (ErrorCode::Unauthorized, close::UNAUTHORIZED),
        (ErrorCode::EmailUnverified, close::EMAIL_UNVERIFIED),
        (ErrorCode::Internal, close::INTERNAL),
        (ErrorCode::ProtocolViolation, close::PROTOCOL_VIOLATION),
        (ErrorCode::CheatDetected, close::CHEAT_DETECTED),
    ];
    for (code, close) in pairs {
        assert_eq!(close_code_for(code), Some(close));
    }
    assert_eq!(error_code_for_close(close::SLOW_CONSUMER), Some(ErrorCode::Unknown(243)));
    assert_eq!(error_code_for_close(close::NORMAL), None);
    assert_eq!(close_code_for(ErrorCode::Unknown(250)), Some(4310));
}

#[test]
fn gesture_relay_is_a_byte_copy() {
    let c = ClientGesture {
        seq: 9,
        game: 77,
        ply: 3,
        touch: 12,
        aim: 28,
        flags: 4,
        yaw: -5,
        pitch: 7,
        lean: 50,
        placed: 0,
    };
    let s = ServerGesture {
        game: 77,
        ply: 3,
        touch: 12,
        aim: 28,
        flags: 4,
        yaw: -5,
        pitch: 7,
        lean: 50,
        placed: 0,
    };
    let frame = c.to_vec().unwrap();
    assert_eq!(ClientGesture::relay_frame(&frame).unwrap().as_slice(), s.to_vec().unwrap());
    let mut bad = frame.clone();
    bad[15] = 65; // touch: after type, seq, game and ply
    assert_eq!(ClientGesture::relay_frame(&bad).unwrap_err().to_string(), "touch above max");
    assert!(ClientGesture::relay_frame(&frame[..frame.len() - 1]).is_err());
}

#[test]
fn message_types_and_sizes() {
    for t in MsgType::ALL {
        assert_eq!(MsgType::from_u8(t.to_u8()), Some(t));
        assert_eq!(t.is_client(), t.to_u8() < 0x80);
        assert!(t.min_len() <= t.max_len());
        let limit = if t.is_client() { MAX_CLIENT_MESSAGE } else { MAX_SERVER_MESSAGE };
        assert!(t.max_len() <= limit, "{}", t.name());
    }
    assert_eq!((Hello::MIN_LEN, Hello::MAX_LEN), (35, 227));
    assert_eq!((Move::MIN_LEN, Move::MAX_LEN), (26, 26));
    assert_eq!(MsgType::GameSnapshot.max_len(), GameSnapshot::MAX_LEN);
    assert_eq!(ServerGesture::MIN_LEN, ServerGesture::MAX_LEN);
    assert_eq!(
        ClientMsg::decode(&ServerPing::default().to_vec().unwrap()).unwrap_err().defect(),
        Defect::WrongDirection
    );
    assert_eq!(ServerMsg::decode(&Resign::default().to_vec().unwrap()), Ok(None));
    assert_eq!(Resign::decode(&[]).unwrap_err().defect(), Defect::Empty);
    assert_eq!(Resign::decode(&Abort::default().to_vec().unwrap()).unwrap_err().defect(), Defect::WrongType);
}

#[test]
fn server_messages_decode_leniently_alone_too() {
    let mut bytes = Ack { r#ref: 3 }.to_vec().unwrap();
    bytes.push(0);
    assert_eq!(Ack::decode(&bytes), Ok(Ack { r#ref: 3 }));
    assert_eq!(Ack::decode_exact(&bytes).unwrap_err().defect(), Defect::TrailingBytes);
    let mut bytes = Error { code: ErrorCode::Flood, ..Error::default() }.to_vec().unwrap();
    bytes[5] = 99;
    assert_eq!(Error::decode(&bytes).map(|e| e.code), Ok(ErrorCode::Unknown(99)));
    let mut bytes = Resign::default().to_vec().unwrap();
    bytes.push(0);
    assert_eq!(Resign::decode(&bytes).unwrap_err().defect(), Defect::TrailingBytes);
}
