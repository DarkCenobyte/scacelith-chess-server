//! Interpreted codec: encodes and decodes messages described by the schema model, with field
//! values as JSON (enums as numbers, structs as objects, lists as arrays). protogen builds and
//! checks the golden vectors with it, independently of the generated codecs it writes; the
//! crate's tests use it to cross-check the generated Rust codec.
//!
//! Primitive reads come from `crate::wire` so that the reasons have the exact wording of the
//! generated codec.

use serde_json::{Map, Value};

use super::model::{Dir, Field, Message, Schema, Type};
use crate::wire::{DecodeError, ID53_LIMIT, Reader, check_text};

/// Result of decoding one message.
#[derive(Clone, Debug, PartialEq)]
pub enum Decoded {
    /// A message: its vector name and fields.
    Message { key: String, fields: Value },
    /// A message a lenient receiver ignores (unknown type, or a type of the other direction).
    Ignored,
}

/// Encodes a message from its fields, validating every value.
pub fn encode(schema: &Schema, m: &Message, fields: &Value) -> Result<Vec<u8>, String> {
    let mut out = vec![m.id];
    write_fields(schema, &m.fields, fields, &m.key, &mut out)?;
    Ok(out)
}

fn write_fields(
    schema: &Schema,
    fields: &[Field],
    obj: &Value,
    path: &str,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let map = obj.as_object().ok_or_else(|| format!("{path}: not an object"))?;
    for key in map.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            return Err(format!("{path}: unknown field {key}"));
        }
    }
    for f in fields {
        let at = format!("{path}.{}", f.name);
        let v = map.get(&f.name).ok_or_else(|| format!("{at}: missing"))?;
        write_value(schema, f, &f.ty, v, &at, out)?;
    }
    Ok(())
}

fn write_value(
    schema: &Schema,
    f: &Field,
    ty: &Type,
    v: &Value,
    at: &str,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let bad = |why: &str| Err(format!("{at}: {why} ({v})"));
    match ty {
        Type::U8 | Type::U16 | Type::U32 | Type::I32 => {
            let Some(n) = v.as_i64() else { return bad("not an integer") };
            if n < f.lo() || n > f.hi() {
                return bad("out of bounds");
            }
            match ty {
                Type::U8 => out.push(n as u8),
                Type::U16 => out.extend_from_slice(&(n as u16).to_le_bytes()),
                Type::U32 => out.extend_from_slice(&(n as u32).to_le_bytes()),
                _ => out.extend_from_slice(&(n as i32).to_le_bytes()),
            }
        }
        Type::U64 => {
            let Some(n) = v.as_u64() else { return bad("not a u64") };
            out.extend_from_slice(&n.to_le_bytes());
        }
        Type::Id53 => {
            let Some(n) = v.as_u64().filter(|&n| n < ID53_LIMIT) else { return bad("not an id53") };
            out.extend_from_slice(&n.to_le_bytes());
        }
        Type::F64 => {
            let Some(x) = v.as_f64().filter(|x| x.is_finite()) else { return bad("not a finite f64") };
            out.extend_from_slice(&(if x == 0.0 { 0.0f64 } else { x }).to_le_bytes());
        }
        Type::Bool => {
            let Some(b) = v.as_bool() else { return bad("not a bool") };
            out.push(u8::from(b));
        }
        Type::Str8 => {
            let Some(s) = v.as_str() else { return bad("not a string") };
            let len = s.len() as i64;
            if len < f.lo() || len > f.hi() || s.contains('\0') {
                return bad("bad string");
            }
            out.push(s.len() as u8);
            out.extend_from_slice(s.as_bytes());
        }
        Type::Enum(name) => {
            let e = schema.enum_(name);
            let Some(n) = v.as_u64().and_then(|n| u8::try_from(n).ok()).filter(|&n| e.by_value(n).is_some())
            else {
                return bad("not a member");
            };
            out.push(n);
        }
        Type::Struct(name) => write_fields(schema, &schema.struct_(name).fields, v, at, out)?,
        Type::List(item) => {
            let Some(items) = v.as_array() else { return bad("not an array") };
            if items.len() as i64 > f.hi() {
                return bad("too long");
            }
            out.extend_from_slice(&(items.len() as u16).to_le_bytes());
            let item_field = Field {
                name: f.name.clone(),
                ty: (**item).clone(),
                min: None,
                max: None,
                doc: String::new(),
            };
            for (i, x) in items.iter().enumerate() {
                write_value(schema, &item_field, item, x, &format!("{at}.{i}"), out)?;
            }
        }
    }
    Ok(())
}

/// Decodes the bytes a receiver gets in direction `dir`; `strict` is false for the lenient
/// decoding of server messages by clients. The error is the stable reason string.
pub fn decode(schema: &Schema, bytes: &[u8], dir: Dir, strict: bool) -> Result<Decoded, String> {
    let Some(&id) = bytes.first() else { return Err("empty".into()) };
    let lenient = !strict && dir == Dir::S2c;
    let m = match schema.message_by_id(id) {
        Some(m) if m.dir == dir => m,
        Some(_) if lenient => return Ok(Decoded::Ignored),
        Some(_) => return Err("wrong direction".into()),
        None if lenient => return Ok(Decoded::Ignored),
        None => return Err("unknown type".into()),
    };
    let mut r = Reader::new(bytes, !lenient);
    let fields = read_fields(schema, &m.fields, "", &mut r)?;
    r.finish().map_err(|e| e.to_string())?;
    Ok(Decoded::Message { key: m.key.clone(), fields })
}

/// Decodes a Hello like a server: strict, except that a Hello of a higher minor than the
/// schema's may carry fields this minor does not know (trailing bytes).
pub fn decode_hello(schema: &Schema, bytes: &[u8]) -> Result<Value, String> {
    let m = schema.message_by_id(1).expect("Hello is frozen at 0x01");
    match bytes.first().map(|&id| schema.message_by_id(id)) {
        Some(Some(h)) if h.id == m.id => {}
        None => return Err("empty".into()),
        Some(None) => return Err("unknown type".into()),
        Some(Some(other)) if other.dir != Dir::C2s => return Err("wrong direction".into()),
        Some(Some(_)) => return Err("wrong type".into()),
    }
    let mut r = Reader::new(bytes, true);
    let fields = read_fields(schema, &m.fields, "", &mut r)?;
    let minor = fields["minor"].as_u64().unwrap_or(0);
    if minor <= u64::from(schema.minor) {
        r.finish().map_err(|e| e.to_string())?;
    }
    Ok(fields)
}

/// Re-attaches the dynamic field path to an error of the primitive reader (which was given a
/// placeholder name).
fn reason(e: DecodeError, path: &str) -> String {
    if e.field().is_empty() { e.defect().to_string() } else { format!("{path} {}", e.defect()) }
}

fn read_fields(schema: &Schema, fields: &[Field], prefix: &str, r: &mut Reader<'_>) -> Result<Value, String> {
    let mut map = Map::new();
    for f in fields {
        let path = format!("{prefix}{}", f.name);
        let v = read_value(schema, f, &f.ty, &path, r)?;
        map.insert(f.name.clone(), v);
    }
    Ok(Value::Object(map))
}

fn read_value(
    schema: &Schema,
    f: &Field,
    ty: &Type,
    path: &str,
    r: &mut Reader<'_>,
) -> Result<Value, String> {
    let e = |err: DecodeError| reason(err, path);
    Ok(match ty {
        Type::U8 => {
            Value::from(Reader::bounded(i64::from(r.u8().map_err(e)?), "x", f.lo(), f.hi()).map_err(e)?)
        }
        Type::U16 => {
            Value::from(Reader::bounded(i64::from(r.u16().map_err(e)?), "x", f.lo(), f.hi()).map_err(e)?)
        }
        Type::U32 => {
            Value::from(Reader::bounded(i64::from(r.u32().map_err(e)?), "x", f.lo(), f.hi()).map_err(e)?)
        }
        Type::I32 => {
            Value::from(Reader::bounded(i64::from(r.i32().map_err(e)?), "x", f.lo(), f.hi()).map_err(e)?)
        }
        Type::U64 => Value::from(r.u64().map_err(e)?),
        Type::Id53 => Value::from(r.id53("x").map_err(e)?),
        Type::F64 => Value::from(r.f64("x").map_err(e)?),
        Type::Bool => Value::from(r.bool("x").map_err(e)?),
        Type::Str8 => {
            let s = r.str8("x", f.lo() as usize, f.hi() as usize).map_err(e)?;
            debug_assert!(check_text(s.as_bytes()).is_ok());
            Value::from(s)
        }
        Type::Enum(name) => {
            let en = schema.enum_(name);
            let v = r.u8().map_err(e)?;
            if en.by_value(v).is_none() && (!en.open || r.is_strict()) {
                return Err(format!("{path} not in {name}"));
            }
            Value::from(v)
        }
        Type::Struct(name) => read_fields(schema, &schema.struct_(name).fields, &format!("{path}."), r)?,
        Type::List(item) => {
            let count = r.count16("x", f.hi() as usize, schema.item_min_size(item)).map_err(e)?;
            let item_field = Field {
                name: f.name.clone(),
                ty: (**item).clone(),
                min: None,
                max: None,
                doc: String::new(),
            };
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(read_value(schema, &item_field, item, path, r)?);
            }
            Value::Array(items)
        }
    })
}

/// Encoded size of a value of a field.
fn size_of(schema: &Schema, ty: &Type, v: &Value) -> usize {
    if let Some(n) = ty.scalar_size() {
        return n;
    }
    match ty {
        Type::Str8 => 1 + v.as_str().map_or(0, str::len),
        Type::Struct(name) => {
            schema.struct_(name).fields.iter().map(|f| size_of(schema, &f.ty, &v[&f.name])).sum()
        }
        Type::List(item) => 2 + v.as_array().map_or(0, |a| a.iter().map(|x| size_of(schema, item, x)).sum()),
        _ => unreachable!("scalars handled above"),
    }
}

/// Byte offset of a field in the encoding of `fields`: `"white.name"`, `"moves.3"` (the start of
/// item 3), `"moves.3.move"`.
pub fn locate(schema: &Schema, m: &Message, fields: &Value, path: &str) -> usize {
    let parts: Vec<&str> = path.split('.').collect();
    let mut offset = 1;
    let mut list: &[Field] = &m.fields;
    let mut obj = fields;
    let mut i = 0;
    loop {
        let mut found = None;
        for f in list {
            if f.name == parts[i] {
                found = Some(f);
                break;
            }
            offset += size_of(schema, &f.ty, &obj[&f.name]);
        }
        let f = found.unwrap_or_else(|| panic!("vectors: no field {path} in {}", m.key));
        if i == parts.len() - 1 {
            return offset;
        }
        match &f.ty {
            Type::Struct(name) => {
                list = &schema.struct_(name).fields;
                obj = &obj[&f.name];
                i += 1;
            }
            Type::List(item) => {
                let index: usize = parts[i + 1].parse().expect("a list index");
                offset += 2;
                for x in obj[&f.name].as_array().expect("a list").iter().take(index) {
                    offset += size_of(schema, item, x);
                }
                if i + 1 == parts.len() - 1 {
                    return offset;
                }
                let Type::Struct(name) = &**item else { panic!("vectors: cannot descend into {path}") };
                list = &schema.struct_(name).fields;
                obj = &obj[&f.name][index];
                i += 2;
            }
            _ => panic!("vectors: cannot descend into {path}"),
        }
    }
}

/// Small deterministic generator (xorshift64*), for random vectors and tests.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`, with the bounds themselves more often.
    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        match self.below(10) {
            0 => lo,
            1 => hi,
            _ => lo + (self.next_u64() % ((hi - lo) as u64 + 1)) as i64,
        }
    }
}

/// Characters of 1, 2, 3 and 4 UTF-8 bytes (a BOM among them: it must survive a round trip).
const CHARS: [&[&str]; 4] = [
    &["a", "Z", "0", " ", "+", "-", "~"],
    &["Ł", "é", "ß", "ж", "م"],
    &["ユ", "キ", "♞", "€", "\u{feff}"],
    &["🐴", "😀", "𝄞"],
];

/// A random valid value of every field of a field list.
pub fn random_fields(schema: &Schema, fields: &[Field], rng: &mut Rng) -> Value {
    let mut map = Map::new();
    for f in fields {
        map.insert(f.name.clone(), random_value(schema, f, &f.ty, rng));
    }
    Value::Object(map)
}

fn random_value(schema: &Schema, f: &Field, ty: &Type, rng: &mut Rng) -> Value {
    match ty {
        Type::U8 | Type::U16 | Type::U32 | Type::I32 => Value::from(rng.int(f.lo(), f.hi())),
        Type::U64 => Value::from(rng.below(1 << 53)),
        Type::Id53 => Value::from(rng.int(0, (ID53_LIMIT - 1) as i64)),
        Type::F64 => Value::from(match rng.below(4) {
            0 => 0.0,
            1 => rng.below(2_000_000_000_000) as f64 + rng.below(4) as f64 / 4.0,
            _ => loop {
                let x = f64::from_bits(rng.next_u64());
                if x.is_finite() && x != 0.0 {
                    break x;
                }
            },
        }),
        Type::Bool => Value::from(rng.below(2) == 1),
        Type::Str8 => {
            let target = rng.int(f.lo(), f.hi().min(f.lo() + 40)) as usize;
            let mut s = String::new();
            while s.len() < target {
                let width = 1 + rng.below((target - s.len()).min(4) as u64) as usize;
                let pool = CHARS[width - 1];
                s.push_str(pool[rng.below(pool.len() as u64) as usize]);
            }
            Value::from(s)
        }
        Type::Enum(name) => {
            let e = schema.enum_(name);
            Value::from(e.values[rng.below(e.values.len() as u64) as usize].value)
        }
        Type::Struct(name) => random_fields(schema, &schema.struct_(name).fields, rng),
        Type::List(item) => {
            let n =
                if rng.below(20) == 0 { f.hi() as usize } else { rng.below(13).min(f.hi() as u64) as usize };
            let item_field = Field {
                name: f.name.clone(),
                ty: (**item).clone(),
                min: None,
                max: None,
                doc: String::new(),
            };
            Value::Array((0..n).map(|_| random_value(schema, &item_field, item, rng)).collect())
        }
    }
}
