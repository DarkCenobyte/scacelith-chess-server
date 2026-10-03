//! Codec runtime shared by the generated code (`gen.rs`) and protogen's interpreter: the
//! little-endian reader with the validation rules of the protocol, the writers, and the error
//! types. See `docs/PROTOCOL.md`, "Encoding".

use std::fmt;

use bytes::BufMut;

/// Game ids (`id53`) are below 2^53, so that JSON numbers carry them exactly.
pub const ID53_LIMIT: u64 = 1 << 53;

/// What is wrong with a message (decoding) or with a value (encoding).
///
/// The `Display` form is stable: the server records it as the reason of a `malformed` anomaly
/// and the golden vectors list it, prefixed by the field path (`"white.name bad length"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Defect {
    /// The message has no type byte.
    Empty,
    /// The type byte names no message of this protocol.
    UnknownType,
    /// The type byte names a message of the other direction.
    WrongDirection,
    /// The type byte names another message than the one asked for.
    WrongType,
    /// The message ends before its last field.
    Truncated,
    /// Bytes follow the last field (strict decoding only).
    TrailingBytes,
    /// An integer below its lower bound.
    BelowMin,
    /// An integer above its upper bound.
    AboveMax,
    /// A bool other than 0 or 1.
    NotBool,
    /// A value that is not a member of the named enum.
    NotInEnum(&'static str),
    /// A NaN or infinite f64.
    NotFinite,
    /// An id53 of 2^53 or more.
    AboveId53,
    /// A string whose byte length is outside its bounds.
    BadLength,
    /// A string with a NUL byte.
    ContainsNul,
    /// A string that is not strict UTF-8.
    NotUtf8,
    /// A list longer than its maximum.
    TooLong,
}

impl fmt::Display for Defect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "empty",
            Self::UnknownType => "unknown type",
            Self::WrongDirection => "wrong direction",
            Self::WrongType => "wrong type",
            Self::Truncated => "truncated",
            Self::TrailingBytes => "trailing bytes",
            Self::BelowMin => "below min",
            Self::AboveMax => "above max",
            Self::NotBool => "not a bool",
            Self::NotInEnum(name) => return write!(f, "not in {name}"),
            Self::NotFinite => "not finite",
            Self::AboveId53 => "above 2^53",
            Self::BadLength => "bad length",
            Self::ContainsNul => "contains NUL",
            Self::NotUtf8 => "not UTF-8",
            Self::TooLong => "too long",
        };
        f.write_str(text)
    }
}

/// A message that does not decode: the defect and the dotted path of the field it is in (empty
/// for defects of the whole message). Its `Display` form is the stable reason string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecodeError {
    defect: Defect,
    field: &'static str,
}

impl DecodeError {
    /// A defect of field `field` (`""` for the whole message).
    pub const fn new(defect: Defect, field: &'static str) -> Self {
        Self { defect, field }
    }

    /// What is wrong.
    pub const fn defect(&self) -> Defect {
        self.defect
    }

    /// Dotted path of the field (`"white.name"`; list items use the list's name), or `""`.
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}", self.defect)
        } else {
            write!(f, "{} {}", self.field, self.defect)
        }
    }
}

impl std::error::Error for DecodeError {}

/// A message value that cannot be encoded (a bound, a string, a non-finite f64...): a bug of the
/// sender, which writes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EncodeError {
    message: &'static str,
    field: &'static str,
    defect: Defect,
}

impl EncodeError {
    /// A defect of field `field` of message (or struct) `message`.
    pub const fn new(message: &'static str, field: &'static str, defect: Defect) -> Self {
        Self { message, field, defect }
    }

    /// Name of the message or struct.
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Name of the field.
    pub const fn field(&self) -> &'static str {
        self.field
    }

    /// What is wrong.
    pub const fn defect(&self) -> Defect {
        self.defect
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot encode {}.{}: {}", self.message, self.field, self.defect)
    }
}

impl std::error::Error for EncodeError {}

/// Strict UTF-8 without NUL: `Ok` with the text, or the defect.
pub fn check_text(bytes: &[u8]) -> Result<&str, Defect> {
    let text = std::str::from_utf8(bytes).map_err(|_| Defect::NotUtf8)?;
    if bytes.contains(&0) { Err(Defect::ContainsNul) } else { Ok(text) }
}

/// Bounds-checked little-endian reader over one message, positioned after the type byte.
///
/// `strict` decoding refuses trailing bytes and values of open enums that this codec does not
/// know; lenient decoding (server messages received by a client) accepts both.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    strict: bool,
}

impl<'a> Reader<'a> {
    /// A reader of `buf` (type byte included) that starts after the type byte.
    pub fn new(buf: &'a [u8], strict: bool) -> Self {
        Self { buf, pos: buf.len().min(1), strict }
    }

    /// Whether this is strict decoding.
    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Ends the message: trailing bytes are a defect in strict decoding only.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.strict && self.remaining() != 0 {
            Err(DecodeError::new(Defect::TrailingBytes, ""))
        } else {
            Ok(())
        }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self.buf.get(self.pos..self.pos + N).ok_or(DecodeError::new(Defect::Truncated, ""))?;
        self.pos += N;
        Ok(bytes.try_into().expect("slice of N bytes"))
    }

    /// A u8.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>()?[0])
    }

    /// A u16.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    /// A u32.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    /// A u64.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    /// An i32.
    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    /// An integer within `min..=max`.
    pub fn bounded<T: PartialOrd>(value: T, field: &'static str, min: T, max: T) -> Result<T, DecodeError> {
        if value < min {
            Err(DecodeError::new(Defect::BelowMin, field))
        } else if value > max {
            Err(DecodeError::new(Defect::AboveMax, field))
        } else {
            Ok(value)
        }
    }

    /// A finite f64.
    pub fn f64(&mut self, field: &'static str) -> Result<f64, DecodeError> {
        let value = f64::from_le_bytes(self.take()?);
        if value.is_finite() { Ok(value) } else { Err(DecodeError::new(Defect::NotFinite, field)) }
    }

    /// An id53 (u64 below 2^53).
    pub fn id53(&mut self, field: &'static str) -> Result<u64, DecodeError> {
        let value = self.u64()?;
        if value < ID53_LIMIT { Ok(value) } else { Err(DecodeError::new(Defect::AboveId53, field)) }
    }

    /// A bool (0 or 1).
    pub fn bool(&mut self, field: &'static str) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::new(Defect::NotBool, field)),
        }
    }

    /// A str8 whose byte length is within `min..=max`: the length is checked before the bytes
    /// are looked for.
    pub fn str8(&mut self, field: &'static str, min: usize, max: usize) -> Result<String, DecodeError> {
        let len = usize::from(self.u8()?);
        if len < min || len > max {
            return Err(DecodeError::new(Defect::BadLength, field));
        }
        let bytes = self.buf.get(self.pos..self.pos + len).ok_or(DecodeError::new(Defect::Truncated, ""))?;
        let text = check_text(bytes).map_err(|defect| DecodeError::new(defect, field))?;
        self.pos += len;
        Ok(text.to_owned())
    }

    /// The count of a list16 of at most `max` items of at least `item_len` bytes each: the count
    /// is checked against `max`, then against the bytes left, before anything is allocated.
    pub fn count16(
        &mut self,
        field: &'static str,
        max: usize,
        item_len: usize,
    ) -> Result<usize, DecodeError> {
        let count = usize::from(self.u16()?);
        if count > max {
            return Err(DecodeError::new(Defect::TooLong, field));
        }
        if count * item_len > self.remaining() {
            return Err(DecodeError::new(Defect::Truncated, ""));
        }
        Ok(count)
    }
}

/// Writes an f64, -0 as +0 (encodings are canonical).
pub fn put_f64<B: BufMut>(out: &mut B, value: f64) {
    out.put_f64_le(if value == 0.0 { 0.0 } else { value });
}

/// Writes a str8 (the length was validated).
pub fn put_str8<B: BufMut>(out: &mut B, text: &str) {
    out.put_u8(u8::try_from(text.len()).expect("validated str8 length"));
    out.put_slice(text.as_bytes());
}

/// Writes the count of a list16 (the length was validated).
pub fn put_count16<B: BufMut>(out: &mut B, count: usize) {
    out.put_u16_le(u16::try_from(count).expect("validated list16 length"));
}

/// Encoding check of an integer bound.
pub fn check_bounds<T: PartialOrd>(
    message: &'static str,
    field: &'static str,
    value: T,
    min: T,
    max: T,
) -> Result<(), EncodeError> {
    if value < min {
        Err(EncodeError::new(message, field, Defect::BelowMin))
    } else if value > max {
        Err(EncodeError::new(message, field, Defect::AboveMax))
    } else {
        Ok(())
    }
}

/// Encoding check of an f64.
pub fn check_f64(message: &'static str, field: &'static str, value: f64) -> Result<(), EncodeError> {
    if value.is_finite() { Ok(()) } else { Err(EncodeError::new(message, field, Defect::NotFinite)) }
}

/// Encoding check of an id53.
pub fn check_id53(message: &'static str, field: &'static str, value: u64) -> Result<(), EncodeError> {
    if value < ID53_LIMIT { Ok(()) } else { Err(EncodeError::new(message, field, Defect::AboveId53)) }
}

/// Encoding check of a str8: byte length within `min..=max`, no NUL.
pub fn check_str8(
    message: &'static str,
    field: &'static str,
    text: &str,
    min: usize,
    max: usize,
) -> Result<(), EncodeError> {
    if text.len() < min || text.len() > max {
        Err(EncodeError::new(message, field, Defect::BadLength))
    } else if text.as_bytes().contains(&0) {
        Err(EncodeError::new(message, field, Defect::ContainsNul))
    } else {
        Ok(())
    }
}

/// Encoding check of an open enum value: `Unknown` values are never sent.
pub fn check_known(
    message: &'static str,
    field: &'static str,
    known: bool,
    name: &'static str,
) -> Result<(), EncodeError> {
    if known { Ok(()) } else { Err(EncodeError::new(message, field, Defect::NotInEnum(name))) }
}

/// Encoding check of a list16 length.
pub fn check_count16(
    message: &'static str,
    field: &'static str,
    len: usize,
    max: usize,
) -> Result<(), EncodeError> {
    if len > max { Err(EncodeError::new(message, field, Defect::TooLong)) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_stable() {
        assert_eq!(DecodeError::new(Defect::BadLength, "white.name").to_string(), "white.name bad length");
        assert_eq!(DecodeError::new(Defect::Truncated, "").to_string(), "truncated");
        assert_eq!(
            DecodeError::new(Defect::NotInEnum("ColorPref"), "color").to_string(),
            "color not in ColorPref"
        );
        assert_eq!(
            EncodeError::new("Welcome", "username", Defect::BadLength).to_string(),
            "cannot encode Welcome.username: bad length"
        );
    }

    #[test]
    fn text_rules() {
        assert_eq!(check_text("Łódź ♞ 🐴".as_bytes()), Ok("Łódź ♞ 🐴"));
        assert_eq!(check_text(b"\xEF\xBB\xBFbom"), Ok("\u{feff}bom"));
        assert_eq!(check_text(b"a\0b"), Err(Defect::ContainsNul));
        for bad in [&b"\xC0\xAF"[..], b"\xED\xA0\x80", b"\xF4\x90\x80\x80", b"\xE3\x81", b"\x80", b"\xFF"] {
            assert_eq!(check_text(bad), Err(Defect::NotUtf8), "{bad:?}");
        }
    }

    #[test]
    fn reader_checks_lengths_before_bytes() {
        let mut r = Reader::new(&[0x10, 9, b'a'], true);
        assert_eq!(r.str8("category", 3, 7), Err(DecodeError::new(Defect::BadLength, "category")));
        let mut r = Reader::new(&[0x10, 4, b'a'], true);
        assert_eq!(r.str8("category", 3, 7), Err(DecodeError::new(Defect::Truncated, "")));
        let mut r = Reader::new(&[0xA0, 0xB1, 0x04], true);
        assert_eq!(r.count16("moves", 1200, 10), Err(DecodeError::new(Defect::TooLong, "moves")));
        let mut r = Reader::new(&[0xA0, 0x02, 0x00, 1, 2, 3], true);
        assert_eq!(r.count16("moves", 1200, 10), Err(DecodeError::new(Defect::Truncated, "")));
    }

    #[test]
    fn negative_zero_is_written_as_zero() {
        let mut out = Vec::new();
        put_f64(&mut out, -0.0);
        assert_eq!(out, [0; 8]);
    }
}
