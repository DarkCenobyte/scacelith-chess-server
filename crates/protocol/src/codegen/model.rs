//! Schema model: parses `protocol/scacelith-v1.json` and validates every rule of the protocol
//! (names, ids and directions, bounds, frozen layouts, sizes). Every problem is reported, not
//! only the first one.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::canon;

/// Direction of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Client to server (type bytes 0x01-0x7F).
    C2s,
    /// Server to client (type bytes 0x80-0xFF).
    S2c,
}

impl Dir {
    /// `"c2s"` or `"s2c"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::C2s => "c2s",
            Self::S2c => "s2c",
        }
    }

    /// The direction of a type byte (`None` for 0).
    pub fn of_id(id: u8) -> Option<Self> {
        match id {
            0 => None,
            1..=0x7f => Some(Self::C2s),
            _ => Some(Self::S2c),
        }
    }
}

/// Type of a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    U8,
    U16,
    U32,
    U64,
    I32,
    F64,
    Id53,
    Bool,
    Str8,
    Enum(String),
    Struct(String),
    List(Box<Type>),
}

impl Type {
    fn parse(text: &str) -> Result<Self, String> {
        if let Some(item) = text.strip_prefix("list16:") {
            let item = Self::parse(item)?;
            return match item {
                Self::List(_) | Self::Str8 => Err(format!("list16 of {} is not supported", item.spelling())),
                _ => Ok(Self::List(Box::new(item))),
            };
        }
        if let Some(name) = text.strip_prefix("enum:") {
            return Ok(Self::Enum(name.to_owned()));
        }
        if let Some(name) = text.strip_prefix("struct:") {
            return Ok(Self::Struct(name.to_owned()));
        }
        Ok(match text {
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "i32" => Self::I32,
            "f64" => Self::F64,
            "id53" => Self::Id53,
            "bool" => Self::Bool,
            "str8" => Self::Str8,
            _ => return Err(format!("unknown type {text:?}")),
        })
    }

    /// The schema spelling (`"u16"`, `"enum:Color"`, `"list16:struct:MoveRec"`).
    pub fn spelling(&self) -> String {
        match self {
            Self::U8 => "u8".into(),
            Self::U16 => "u16".into(),
            Self::U32 => "u32".into(),
            Self::U64 => "u64".into(),
            Self::I32 => "i32".into(),
            Self::F64 => "f64".into(),
            Self::Id53 => "id53".into(),
            Self::Bool => "bool".into(),
            Self::Str8 => "str8".into(),
            Self::Enum(name) => format!("enum:{name}"),
            Self::Struct(name) => format!("struct:{name}"),
            Self::List(item) => format!("list16:{}", item.spelling()),
        }
    }

    /// Range of a bounded integer type (`None` for the types without bounds).
    pub fn int_range(&self) -> Option<(i64, i64)> {
        match self {
            Self::U8 => Some((0, 0xff)),
            Self::U16 => Some((0, 0xffff)),
            Self::U32 => Some((0, 0xffff_ffff)),
            Self::I32 => Some((i64::from(i32::MIN), i64::from(i32::MAX))),
            _ => None,
        }
    }

    /// Size of a fixed-size scalar (`None` for str8, struct, list).
    pub fn scalar_size(&self) -> Option<usize> {
        match self {
            Self::U8 | Self::Bool | Self::Enum(_) => Some(1),
            Self::U16 => Some(2),
            Self::U32 | Self::I32 => Some(4),
            Self::U64 | Self::F64 | Self::Id53 => Some(8),
            Self::Str8 | Self::Struct(_) | Self::List(_) => None,
        }
    }
}

/// A field of a struct or a message.
#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub doc: String,
}

impl Field {
    /// The description of the field: its `doc`, or the common one of `seq`.
    pub fn description(&self) -> &str {
        match (self.doc.is_empty(), self.name.as_str()) {
            (true, "seq") => {
                "number of the message on its connection: 1 for the Hello, then one more per message"
            }
            _ => &self.doc,
        }
    }

    /// Lower bound of an integer, or of a string's byte length.
    pub fn lo(&self) -> i64 {
        self.min.unwrap_or_else(|| self.ty.int_range().map_or(0, |r| r.0))
    }

    /// Upper bound of an integer, of a string's byte length (255) or of a list's count.
    pub fn hi(&self) -> i64 {
        self.max.unwrap_or_else(|| match &self.ty {
            Type::Str8 => 255,
            Type::List(_) => 0xffff,
            ty => ty.int_range().map_or(0, |r| r.1),
        })
    }

    /// Whether the field has a bound narrower than its type.
    pub fn is_bounded(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    fn same_wire(&self, other: &Field) -> bool {
        self.name == other.name && self.ty == other.ty && self.min == other.min && self.max == other.max
    }
}

/// A struct (fields inline, no header).
#[derive(Clone, Debug)]
pub struct Struct {
    pub name: String,
    pub doc: String,
    pub fields: Vec<Field>,
}

/// A message.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: u8,
    pub name: String,
    pub dir: Dir,
    pub doc: String,
    /// The client message this server message relays byte for byte (without its seq).
    pub relay_of: Option<String>,
    pub fields: Vec<Field>,
    /// Name in the vectors and the C++ codec: the schema name, prefixed with `C_` or `S_` when
    /// both directions have a message of that name.
    pub key: String,
    /// Whether the other direction has a message of the same name.
    pub shared: bool,
}

/// A member (or a retired value) of an enum.
#[derive(Clone, Debug)]
pub struct EnumValue {
    pub name: String,
    pub value: u8,
    pub doc: String,
}

/// An enum (u8 on the wire).
#[derive(Clone, Debug)]
pub struct Enum {
    pub name: String,
    /// Open enums may gain values in a later minor: receivers keep unknown values.
    pub open: bool,
    pub doc: String,
    /// What a receiver does with a value it does not know (open enums).
    pub unknown_doc: String,
    pub values: Vec<EnumValue>,
    /// Retired values, never reused.
    pub reserved: Vec<EnumValue>,
}

impl Enum {
    /// The member of a value.
    pub fn by_value(&self, value: u8) -> Option<&EnumValue> {
        self.values.iter().find(|v| v.value == value)
    }

    /// The member of a name.
    pub fn by_name(&self, name: &str) -> Option<&EnumValue> {
        self.values.iter().find(|v| v.name == name)
    }

    /// Whether the values are 0..n-1 or first..last without a gap.
    pub fn contiguous(&self) -> bool {
        let min = self.values.iter().map(|v| v.value).min().unwrap_or(0);
        let max = self.values.iter().map(|v| v.value).max().unwrap_or(0);
        usize::from(max - min) + 1 == self.values.len()
    }
}

/// A bit of a flag set.
#[derive(Clone, Debug)]
pub struct FlagBit {
    pub name: String,
    pub value: u8,
    pub doc: String,
}

/// A set of bits in a u8 field.
#[derive(Clone, Debug)]
pub struct FlagSet {
    pub name: String,
    pub doc: String,
    pub bits: Vec<FlagBit>,
}

/// A WebSocket close code.
#[derive(Clone, Debug)]
pub struct CloseCode {
    pub name: String,
    pub code: u16,
    /// The ErrorCode member whose fatal Error precedes this close.
    pub error: Option<String>,
    pub doc: String,
}

/// A named protocol constant.
#[derive(Clone, Debug)]
pub struct Constant {
    pub name: String,
    pub value: u64,
    pub doc: String,
}

/// A range of type bytes.
#[derive(Clone, Debug)]
pub struct Range {
    pub first: u8,
    pub last: u8,
    pub dir: Dir,
    pub area: String,
    /// False for the experimental ranges, never used by a published minor.
    pub published: bool,
}

/// A capability bit.
#[derive(Clone, Debug)]
pub struct Cap {
    pub name: String,
    pub bit: u8,
    pub doc: String,
}

/// The validated schema.
#[derive(Clone, Debug)]
pub struct Schema {
    pub about: String,
    pub protocol: u16,
    pub minor: u16,
    pub subprotocol: String,
    pub constants: Vec<Constant>,
    pub ranges: Vec<Range>,
    pub caps: Vec<Cap>,
    pub enums: Vec<Enum>,
    pub flags: Vec<FlagSet>,
    pub close_codes: Vec<CloseCode>,
    pub structs: Vec<Struct>,
    pub messages: Vec<Message>,
    /// Canonical JSON of the wire part of the schema (prose left out).
    pub canonical: String,
    /// First four bytes (big-endian) of SHA-256 of `canonical`.
    pub fingerprint: u32,
}

/// Close code of the fatal Error of an ErrorCode value: 4000 + code (1..99) or
/// 4300 + (code - 240) (240..255).
pub fn close_rule(code: u8) -> Option<u16> {
    match code {
        1..=99 => Some(4000 + u16::from(code)),
        240..=255 => Some(4300 + u16::from(code - 240)),
        _ => None,
    }
}

/// A layout every version keeps (a compatibility anchor).
struct FrozenLayout {
    id: u8,
    /// The first fields of the message, as `(name, type)`.
    fields: &'static [(&'static str, &'static str)],
    /// Whether these are all its fields.
    whole: bool,
}

/// The compatibility anchors of protocol 1.
const FROZEN_LAYOUTS: &[FrozenLayout] = &[
    FrozenLayout {
        id: 0x01,
        fields: &[("seq", "u32"), ("proto", "u16"), ("minor", "u16"), ("caps", "u64")],
        whole: false,
    },
    FrozenLayout { id: 0x02, fields: &[("seq", "u32"), ("nonce", "u32")], whole: true },
    FrozenLayout { id: 0x03, fields: &[("seq", "u32"), ("nonce", "u32")], whole: true },
    FrozenLayout { id: 0x80, fields: &[("proto", "u16"), ("minor", "u16"), ("caps", "u64")], whole: false },
    FrozenLayout {
        id: 0x81,
        fields: &[("ref", "u32"), ("code", "enum:ErrorCode"), ("fatal", "bool"), ("game", "id53")],
        whole: true,
    },
    FrozenLayout { id: 0x82, fields: &[("nonce", "u32"), ("serverTime", "f64")], whole: true },
    FrozenLayout { id: 0x83, fields: &[("nonce", "u32"), ("serverTime", "f64")], whole: true },
];

impl Schema {
    /// Parses and validates the schema text.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        Self::parse_with(text, true)
    }

    /// Parses and validates a frozen manifest: a schema without its prose.
    pub fn parse_frozen(text: &str) -> Result<Self, Vec<String>> {
        Self::parse_with(text, false)
    }

    fn parse_with(text: &str, prose: bool) -> Result<Self, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| vec![format!("schema: invalid JSON: {e}")])?;
        let mut p = Parser { errors: Vec::new() };
        let schema = p.schema(&value);
        if !p.errors.is_empty() {
            return Err(p.errors);
        }
        let mut schema = schema.expect("a schema without errors");
        let errors = schema.validate(prose);
        if !errors.is_empty() {
            return Err(errors);
        }
        schema.canonical = canon::canonical_json(&value);
        schema.fingerprint = canon::fingerprint(&schema.canonical);
        Ok(schema)
    }

    /// The enum of a name.
    pub fn enum_(&self, name: &str) -> &Enum {
        self.enums.iter().find(|e| e.name == name).expect("validated enum reference")
    }

    /// The struct of a name.
    pub fn struct_(&self, name: &str) -> &Struct {
        self.structs.iter().find(|s| s.name == name).expect("validated struct reference")
    }

    /// The message of a vector name (`"Move"`, `"C_Ping"`).
    pub fn message(&self, key: &str) -> Option<&Message> {
        self.messages.iter().find(|m| m.key == key)
    }

    /// The message of a type byte.
    pub fn message_by_id(&self, id: u8) -> Option<&Message> {
        self.messages.iter().find(|m| m.id == id)
    }

    /// Value of a constant.
    pub fn constant(&self, name: &str) -> u64 {
        self.constants.iter().find(|c| c.name == name).map(|c| c.value).expect("validated constant")
    }

    /// Smallest and largest encoded size of a type.
    pub fn type_size(&self, field: &Field) -> (usize, usize) {
        self.ty_size(&field.ty, field.lo(), field.hi())
    }

    /// Sizes of a value of `ty` whose length (str8) or count (list16) is within `lo..=hi`.
    fn ty_size(&self, ty: &Type, lo: i64, hi: i64) -> (usize, usize) {
        if let Some(n) = ty.scalar_size() {
            return (n, n);
        }
        let (lo, hi) = (usize::try_from(lo).unwrap_or(0), usize::try_from(hi).unwrap_or(0));
        match ty {
            Type::Str8 => (1 + lo, 1 + hi),
            Type::Struct(name) => self.fields_size(&self.struct_(name).fields),
            Type::List(item) => {
                let (item_min, item_max) = self.ty_size(item, 0, 255);
                (2 + lo * item_min, 2 + hi * item_max)
            }
            _ => unreachable!("scalar handled above"),
        }
    }

    /// Smallest and largest size of a field list.
    pub fn fields_size(&self, fields: &[Field]) -> (usize, usize) {
        fields.iter().map(|f| self.type_size(f)).fold((0, 0), |(a, b), (c, d)| (a + c, b + d))
    }

    /// Smallest and largest size of a message, type byte included.
    pub fn message_size(&self, m: &Message) -> (usize, usize) {
        let (min, max) = self.fields_size(&m.fields);
        (1 + min, 1 + max)
    }

    /// Smallest size of an item of a list (for the truncation check before allocation).
    pub fn item_min_size(&self, item: &Type) -> usize {
        self.ty_size(item, 0, 255).0
    }

    /// The range a type byte falls in.
    pub fn range_of(&self, id: u8) -> Option<&Range> {
        self.ranges.iter().find(|r| r.first <= id && id <= r.last)
    }

    /// Every rule of the schema; `prose`: the descriptions the rules ask for are present too.
    fn validate(&mut self, prose: bool) -> Vec<String> {
        let mut errors = Vec::new();
        let mut err = |e: String| errors.push(e);

        if self.protocol == 0 {
            err("protocol: must be at least 1".into());
        }
        let token_char = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        if self.subprotocol.is_empty() || !self.subprotocol.chars().all(token_char) {
            err(format!("subprotocol: {:?} is not an RFC 7230 token", self.subprotocol));
        }

        // Constants.
        let mut seen = HashSet::new();
        for c in &self.constants {
            if !is_pascal(&c.name) || !seen.insert(c.name.as_str()) {
                err(format!("constants: bad or duplicate name {:?}", c.name));
            }
        }
        for required in ["MaxClientMessage", "MaxServerMessage"] {
            if !seen.contains(required) {
                err(format!("constants: {required} is required"));
            }
        }

        // Ranges: 1..=255 covered once, each range in one direction.
        let mut covered = [false; 256];
        for r in &self.ranges {
            if r.first == 0
                || r.first > r.last
                || Dir::of_id(r.first) != Some(r.dir)
                || Dir::of_id(r.last) != Some(r.dir)
            {
                err(format!(
                    "ranges: {:#04x}..{:#04x} is not a range of {}",
                    r.first,
                    r.last,
                    r.dir.as_str()
                ));
            }
            for id in r.first..=r.last {
                if std::mem::replace(&mut covered[usize::from(id)], true) {
                    err(format!("ranges: {id:#04x} is in two ranges"));
                }
            }
        }
        if let Some(id) = (1..=255usize).find(|&id| !covered[id]) {
            err(format!("ranges: {id:#04x} is in no range"));
        }

        // Capabilities.
        let mut bits = HashSet::new();
        let mut names = HashSet::new();
        for c in &self.caps {
            if c.bit > 63 || !bits.insert(c.bit) || !is_pascal(&c.name) || !names.insert(c.name.as_str()) {
                err(format!("caps: bad or duplicate capability {:?} (bit {})", c.name, c.bit));
            }
        }

        // Enums.
        let mut type_names = HashSet::new();
        for e in &self.enums {
            if !is_pascal(&e.name) || !type_names.insert(e.name.clone()) {
                err(format!("enums: bad or duplicate name {:?}", e.name));
            }
            if e.values.is_empty() {
                err(format!("enum {}: no value", e.name));
            }
            let mut values = HashSet::new();
            let mut names = HashSet::new();
            for v in e.values.iter().chain(&e.reserved) {
                if !values.insert(v.value) {
                    err(format!("enum {}: value {} used twice", e.name, v.value));
                }
                if !is_pascal(&v.name) || !names.insert(v.name.as_str()) {
                    err(format!("enum {}: bad or duplicate name {:?}", e.name, v.name));
                }
            }
            if prose && e.open && e.unknown_doc.is_empty() {
                err(format!(
                    "enum {}: an open enum says what a receiver does with an unknown value (unknownDoc)",
                    e.name
                ));
            }
            if !e.open && !e.unknown_doc.is_empty() {
                err(format!("enum {}: unknownDoc on a closed enum", e.name));
            }
        }

        // Flags.
        for f in &self.flags {
            if !is_pascal(&f.name) || !type_names.insert(f.name.clone()) {
                err(format!("flags: bad or duplicate name {:?}", f.name));
            }
            let mut used = 0u8;
            for b in &f.bits {
                if !b.value.is_power_of_two() || used & b.value != 0 || !is_pascal(&b.name) {
                    err(format!("flags {}: bad bit {} ({})", f.name, b.name, b.value));
                }
                used |= b.value;
            }
        }

        // Close codes and the close rule.
        let error_enum = self.enums.iter().find(|e| e.name == "ErrorCode");
        if error_enum.is_none() {
            err("enums: ErrorCode is required".into());
        }
        let mut codes = HashSet::new();
        let mut names = HashSet::new();
        for c in &self.close_codes {
            if !codes.insert(c.code) || !is_pascal(&c.name) || !names.insert(c.name.as_str()) {
                err(format!("closeCodes: bad or duplicate {} ({})", c.name, c.code));
            }
            if !(1000..=1015).contains(&c.code) && !(4000..=4999).contains(&c.code) {
                err(format!("closeCodes: {} ({}) is neither a WebSocket nor a private code", c.name, c.code));
            }
            match (&c.error, error_enum) {
                (Some(name), Some(e)) => match e.by_name(name) {
                    Some(v) if close_rule(v.value) == Some(c.code) => {}
                    Some(v) => err(format!(
                        "closeCodes: {} ({}) breaks the close rule: ErrorCode {} closes with {:?}",
                        c.name,
                        c.code,
                        v.name,
                        close_rule(v.value)
                    )),
                    None => err(format!("closeCodes: {}: no ErrorCode {name}", c.name)),
                },
                (None, Some(e)) if (4000..=4999).contains(&c.code) => {
                    // A private code without an Error must still follow the rule through a
                    // reserved ErrorCode value.
                    let reserved = e.reserved.iter().any(|v| close_rule(v.value) == Some(c.code));
                    if !reserved {
                        err(format!("closeCodes: {} ({}) maps to no ErrorCode value", c.name, c.code));
                    }
                }
                _ => {}
            }
        }

        // Structs.
        for s in &self.structs {
            if !is_pascal(&s.name) || !type_names.insert(s.name.clone()) {
                err(format!("structs: bad or duplicate name {:?}", s.name));
            }
        }
        let structs: HashMap<&str, &Struct> = self.structs.iter().map(|s| (s.name.as_str(), s)).collect();
        let enums: HashSet<&str> = self.enums.iter().map(|e| e.name.as_str()).collect();
        for s in &self.structs {
            self.check_fields(&format!("struct {}", s.name), &s.fields, &enums, &structs, &mut errors);
            if self.struct_cycle(&s.name, &structs, &mut Vec::new()) {
                errors.push(format!("struct {}: contains itself", s.name));
            }
        }

        // Messages.
        let mut ids = HashSet::new();
        let mut by_dir: HashMap<(Dir, &str), u8> = HashMap::new();
        for m in &self.messages {
            let what = format!("message {} ({:#04x})", m.name, m.id);
            if !ids.insert(m.id) {
                errors.push(format!("{what}: id used twice"));
            }
            if Dir::of_id(m.id) != Some(m.dir) {
                errors.push(format!("{what}: id is not in the {} range", m.dir.as_str()));
            }
            match self.range_of(m.id) {
                Some(r) if !r.published => {
                    errors.push(format!("{what}: id is in the {} range, never published", r.area))
                }
                _ => {}
            }
            if !is_pascal(&m.name) || by_dir.insert((m.dir, m.name.as_str()), m.id).is_some() {
                errors.push(format!("{what}: bad name, or a second message of that name in this direction"));
            }
            self.check_fields(&what, &m.fields, &enums, &structs, &mut errors);
            if m.dir == Dir::C2s {
                match m.fields.first() {
                    Some(f) if f.name == "seq" && f.ty == Type::U32 && !f.is_bounded() => {}
                    _ => errors.push(format!("{what}: a client message starts with seq u32 (no bounds)")),
                }
            }
            if m.relay_of.is_some() && m.dir != Dir::S2c {
                errors.push(format!("{what}: only a server message relays a client message"));
            }
        }
        for m in self.messages.iter().filter(|m| m.relay_of.is_some()) {
            let name = m.relay_of.as_deref().unwrap_or_default();
            match self.messages.iter().find(|c| c.dir == Dir::C2s && c.name == name) {
                Some(c) => {
                    let same = c.fields.len() == m.fields.len() + 1
                        && c.fields[1..].iter().zip(&m.fields).all(|(a, b)| a.same_wire(b));
                    if !same {
                        errors.push(format!(
                            "message {}: relays client {name}, so its fields are the client's without seq",
                            m.name
                        ));
                    }
                    let (min, max) = self.fields_size(&m.fields);
                    if min != max {
                        errors.push(format!("message {}: a relayed message has a fixed size", m.name));
                    }
                }
                None => errors.push(format!("message {}: relays no client message {name:?}", m.name)),
            }
        }

        // Frozen layouts.
        for FrozenLayout { id, fields: prefix, whole } in FROZEN_LAYOUTS {
            let Some(m) = self.messages.iter().find(|m| m.id == *id) else {
                errors.push(format!("message {id:#04x} is frozen and required"));
                continue;
            };
            let have: Vec<(String, String)> =
                m.fields.iter().map(|f| (f.name.clone(), f.ty.spelling())).collect();
            let want: Vec<(String, String)> =
                prefix.iter().map(|(n, t)| ((*n).into(), (*t).into())).collect();
            let ok = have.len() >= want.len()
                && have[..want.len()] == want[..]
                && (!whole || have.len() == want.len())
                && m.fields[..want.len()].iter().all(|f| !f.is_bounded());
            if !ok {
                errors.push(format!("message {} ({id:#04x}): frozen layout {want:?} changed", m.name));
            }
        }

        // Sizes.
        if errors.is_empty() {
            let max_c2s = self.constant("MaxClientMessage");
            let max_s2c = self.constant("MaxServerMessage");
            for m in &self.messages {
                let (_, max) = self.message_size(m);
                let limit = if m.dir == Dir::C2s { max_c2s } else { max_s2c };
                if max as u64 > limit {
                    errors.push(format!("message {}: up to {max} bytes, above the limit of {limit}", m.name));
                }
            }
        }

        // Vector keys.
        if errors.is_empty() {
            let shared: HashSet<String> = self
                .messages
                .iter()
                .filter(|m| self.messages.iter().any(|o| o.dir != m.dir && o.name == m.name))
                .map(|m| m.name.clone())
                .collect();
            for m in &mut self.messages {
                m.shared = shared.contains(&m.name);
                let prefix = match (m.shared, m.dir) {
                    (false, _) => "",
                    (true, Dir::C2s) => "C_",
                    (true, Dir::S2c) => "S_",
                };
                m.key = format!("{prefix}{}", m.name);
            }
        }
        errors
    }

    fn struct_cycle<'a>(
        &'a self,
        name: &'a str,
        structs: &HashMap<&str, &'a Struct>,
        path: &mut Vec<&'a str>,
    ) -> bool {
        if path.contains(&name) {
            return true;
        }
        let Some(s) = structs.get(name) else { return false };
        path.push(name);
        let cycle = s.fields.iter().any(|f| {
            let mut ty = &f.ty;
            while let Type::List(item) = ty {
                ty = item;
            }
            matches!(ty, Type::Struct(n) if self.struct_cycle(n, structs, path))
        });
        path.pop();
        cycle
    }

    fn check_fields(
        &self,
        what: &str,
        fields: &[Field],
        enums: &HashSet<&str>,
        structs: &HashMap<&str, &Struct>,
        errors: &mut Vec<String>,
    ) {
        let mut names = HashSet::new();
        for f in fields {
            let at = format!("{what}: field {}", f.name);
            if !is_camel(&f.name) || !names.insert(f.name.as_str()) {
                errors.push(format!("{at}: bad or duplicate name"));
            }
            if f.name == "type" {
                errors.push(format!("{at}: the name `type` is reserved"));
            }
            let mut ty = &f.ty;
            if let Type::List(item) = ty {
                if f.max.is_none() || f.min.is_some() {
                    errors.push(format!("{at}: a list16 has a max and no min"));
                }
                ty = item;
            }
            match ty {
                Type::Enum(name) if !enums.contains(name.as_str()) => {
                    errors.push(format!("{at}: no enum {name}"))
                }
                Type::Struct(name) if !structs.contains_key(name.as_str()) => {
                    errors.push(format!("{at}: no struct {name}"))
                }
                _ => {}
            }
            let (lo, hi) = match &f.ty {
                Type::Str8 => (0, 255),
                Type::List(_) => (0, 0xffff),
                ty => match ty.int_range() {
                    Some(r) => r,
                    None => {
                        if f.is_bounded() {
                            errors.push(format!("{at}: {} takes no bounds", f.ty.spelling()));
                        }
                        continue;
                    }
                },
            };
            let (min, max) = (f.min.unwrap_or(lo), f.max.unwrap_or(hi));
            if min < lo || max > hi || min > max {
                errors.push(format!("{at}: bounds {min}..{max} outside {lo}..{hi}"));
            }
        }
    }
}

/// Collects parse errors while reading the JSON tree.
struct Parser {
    errors: Vec<String>,
}

/// One JSON object being read, with its path for the messages.
struct Obj<'a> {
    map: &'a Map<String, Value>,
    path: String,
}

impl Parser {
    fn obj<'a>(&mut self, value: &'a Value, path: &str, allowed: &[&str]) -> Option<Obj<'a>> {
        let Some(map) = value.as_object() else {
            self.errors.push(format!("{path}: not an object"));
            return None;
        };
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                self.errors.push(format!("{path}: unknown key {key:?}"));
            }
        }
        Some(Obj { map, path: path.to_owned() })
    }

    fn str(&mut self, o: &Obj<'_>, key: &str) -> String {
        match o.map.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => {
                self.errors.push(format!("{}: {key} must be a string", o.path));
                String::new()
            }
        }
    }

    fn opt_str(&mut self, o: &Obj<'_>, key: &str) -> String {
        match o.map.get(key) {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                self.errors.push(format!("{}: {key} must be a string", o.path));
                String::new()
            }
        }
    }

    fn int(&mut self, o: &Obj<'_>, key: &str, lo: i64, hi: i64) -> i64 {
        match o.map.get(key).and_then(Value::as_i64) {
            Some(v) if (lo..=hi).contains(&v) => v,
            _ => {
                self.errors.push(format!("{}: {key} must be an integer in {lo}..{hi}", o.path));
                lo
            }
        }
    }

    fn opt_int(&mut self, o: &Obj<'_>, key: &str) -> Option<i64> {
        let v = o.map.get(key)?;
        let n = v.as_i64();
        if n.is_none() {
            self.errors.push(format!("{}: {key} must be an integer", o.path));
        }
        n
    }

    fn opt_bool(&mut self, o: &Obj<'_>, key: &str, default: bool) -> bool {
        match o.map.get(key) {
            None => default,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                self.errors.push(format!("{}: {key} must be a bool", o.path));
                default
            }
        }
    }

    fn list<'a>(&mut self, o: &Obj<'a>, key: &str) -> &'a [Value] {
        match o.map.get(key) {
            Some(Value::Array(a)) => a,
            None => &[],
            Some(_) => {
                self.errors.push(format!("{}: {key} must be an array", o.path));
                &[]
            }
        }
    }

    fn schema(&mut self, value: &Value) -> Option<Schema> {
        let keys = [
            "about",
            "protocol",
            "minor",
            "subprotocol",
            "constants",
            "ranges",
            "caps",
            "enums",
            "flags",
            "closeCodes",
            "structs",
            "messages",
        ];
        let o = self.obj(value, "schema", &keys)?;
        let about = self.opt_str(&o, "about");
        let protocol = self.int(&o, "protocol", 0, 0xffff) as u16;
        let minor = self.int(&o, "minor", 0, 0xffff) as u16;
        let subprotocol = self.str(&o, "subprotocol");

        let mut constants = Vec::new();
        for (i, v) in self.list(&o, "constants").iter().enumerate() {
            if let Some(c) = self.obj(v, &format!("constants[{i}]"), &["name", "value", "doc"]) {
                let value = self.int(&c, "value", 0, i64::MAX) as u64;
                constants.push(Constant { name: self.str(&c, "name"), value, doc: self.opt_str(&c, "doc") });
            }
        }
        let mut ranges = Vec::new();
        for (i, v) in self.list(&o, "ranges").iter().enumerate() {
            let keys = ["first", "last", "dir", "area", "published", "doc"];
            if let Some(r) = self.obj(v, &format!("ranges[{i}]"), &keys) {
                let dir = self.dir(&r);
                ranges.push(Range {
                    first: self.int(&r, "first", 0, 255) as u8,
                    last: self.int(&r, "last", 0, 255) as u8,
                    dir,
                    area: self.str(&r, "area"),
                    published: self.opt_bool(&r, "published", true),
                });
            }
        }
        let mut caps = Vec::new();
        for (i, v) in self.list(&o, "caps").iter().enumerate() {
            if let Some(c) = self.obj(v, &format!("caps[{i}]"), &["name", "bit", "doc"]) {
                let bit = self.int(&c, "bit", 0, 63) as u8;
                caps.push(Cap { name: self.str(&c, "name"), bit, doc: self.opt_str(&c, "doc") });
            }
        }
        let mut enums = Vec::new();
        for (i, v) in self.list(&o, "enums").iter().enumerate() {
            let keys = ["name", "open", "doc", "unknownDoc", "values", "reserved"];
            if let Some(e) = self.obj(v, &format!("enums[{i}]"), &keys) {
                let name = self.str(&e, "name");
                let values = self.enum_values(&e, "values");
                let reserved = self.enum_values(&e, "reserved");
                enums.push(Enum {
                    open: self.opt_bool(&e, "open", false),
                    doc: self.opt_str(&e, "doc"),
                    unknown_doc: self.opt_str(&e, "unknownDoc"),
                    name,
                    values,
                    reserved,
                });
            }
        }
        let mut flags = Vec::new();
        for (i, v) in self.list(&o, "flags").iter().enumerate() {
            if let Some(f) = self.obj(v, &format!("flags[{i}]"), &["name", "doc", "bits"]) {
                let mut bits = Vec::new();
                for (j, b) in self.list(&f, "bits").iter().enumerate() {
                    if let Some(b) = self.obj(b, &format!("{}.bits[{j}]", f.path), &["name", "value", "doc"])
                    {
                        let value = self.int(&b, "value", 1, 255) as u8;
                        bits.push(FlagBit {
                            name: self.str(&b, "name"),
                            value,
                            doc: self.opt_str(&b, "doc"),
                        });
                    }
                }
                flags.push(FlagSet { name: self.str(&f, "name"), doc: self.opt_str(&f, "doc"), bits });
            }
        }
        let mut close_codes = Vec::new();
        for (i, v) in self.list(&o, "closeCodes").iter().enumerate() {
            if let Some(c) = self.obj(v, &format!("closeCodes[{i}]"), &["name", "code", "error", "doc"]) {
                let code = self.int(&c, "code", 1000, 4999) as u16;
                let error = c.map.contains_key("error").then(|| self.str(&c, "error"));
                close_codes.push(CloseCode {
                    name: self.str(&c, "name"),
                    code,
                    error,
                    doc: self.opt_str(&c, "doc"),
                });
            }
        }
        let mut structs = Vec::new();
        for (i, v) in self.list(&o, "structs").iter().enumerate() {
            if let Some(s) = self.obj(v, &format!("structs[{i}]"), &["name", "doc", "fields"]) {
                let fields = self.fields(&s);
                structs.push(Struct { name: self.str(&s, "name"), doc: self.opt_str(&s, "doc"), fields });
            }
        }
        let mut messages = Vec::new();
        for (i, v) in self.list(&o, "messages").iter().enumerate() {
            let keys = ["id", "name", "dir", "doc", "relayOf", "fields"];
            if let Some(m) = self.obj(v, &format!("messages[{i}]"), &keys) {
                let dir = self.dir(&m);
                let fields = self.fields(&m);
                let relay_of = m.map.contains_key("relayOf").then(|| self.str(&m, "relayOf"));
                messages.push(Message {
                    id: self.int(&m, "id", 1, 255) as u8,
                    name: self.str(&m, "name"),
                    dir,
                    doc: self.opt_str(&m, "doc"),
                    relay_of,
                    fields,
                    key: String::new(),
                    shared: false,
                });
            }
        }
        Some(Schema {
            about,
            protocol,
            minor,
            subprotocol,
            constants,
            ranges,
            caps,
            enums,
            flags,
            close_codes,
            structs,
            messages,
            canonical: String::new(),
            fingerprint: 0,
        })
    }

    fn dir(&mut self, o: &Obj<'_>) -> Dir {
        match self.str(o, "dir").as_str() {
            "c2s" => Dir::C2s,
            "s2c" => Dir::S2c,
            other => {
                self.errors.push(format!("{}: dir {other:?} is neither c2s nor s2c", o.path));
                Dir::C2s
            }
        }
    }

    fn enum_values(&mut self, e: &Obj<'_>, key: &str) -> Vec<EnumValue> {
        let mut out = Vec::new();
        for (j, v) in self.list(e, key).iter().enumerate() {
            if let Some(v) = self.obj(v, &format!("{}.{key}[{j}]", e.path), &["name", "value", "doc"]) {
                let value = self.int(&v, "value", 0, 255) as u8;
                out.push(EnumValue { name: self.str(&v, "name"), value, doc: self.opt_str(&v, "doc") });
            }
        }
        out
    }

    fn fields(&mut self, o: &Obj<'_>) -> Vec<Field> {
        let mut out = Vec::new();
        for (j, v) in self.list(o, "fields").iter().enumerate() {
            let path = format!("{}.fields[{j}]", o.path);
            if let Some(f) = self.obj(v, &path, &["name", "type", "min", "max", "doc"]) {
                let ty = match Type::parse(&self.str(&f, "type")) {
                    Ok(ty) => ty,
                    Err(e) => {
                        self.errors.push(format!("{path}: {e}"));
                        Type::U8
                    }
                };
                out.push(Field {
                    name: self.str(&f, "name"),
                    ty,
                    min: self.opt_int(&f, "min"),
                    max: self.opt_int(&f, "max"),
                    doc: self.opt_str(&f, "doc"),
                });
            }
        }
        out
    }
}

fn is_pascal(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase()) && name.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_camel(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_rule_maps_both_ranges() {
        assert_eq!(close_rule(2), Some(4002));
        assert_eq!(close_rule(11), Some(4011));
        assert_eq!(close_rule(242), Some(4302));
        assert_eq!(close_rule(100), None);
    }

    #[test]
    fn types_parse_and_spell() {
        for t in [
            "u8",
            "u16",
            "u32",
            "u64",
            "i32",
            "f64",
            "id53",
            "bool",
            "str8",
            "enum:Color",
            "list16:struct:MoveRec",
        ] {
            assert_eq!(Type::parse(t).unwrap().spelling(), t);
        }
        assert!(Type::parse("list16:str8").is_err());
        assert!(Type::parse("u128").is_err());
    }
}
