//! Strict body schemas (docs/API.md 1.2): unknown fields are refused, fields are required unless
//! optional, types and lengths are checked, strings refuse control characters. The messages are
//! those of the Node server, word for word; lengths count UTF-16 code units as JavaScript does.
//!
//! ```ignore
//! let schema = Schema::new()
//!     .field("name", Spec::string().min(2).max(8).pattern(|s| s.bytes().all(|b| b.is_ascii_lowercase())))
//!     .field("n", Spec::integer().min(1.0).max(9.0).optional());
//! let body = schema.validate(&value)?;
//! ```

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::json::{js_keys, js_number, safe_integer};

/// A check of a string's format (the anchored regular expressions of the Node schemas).
pub type Pattern = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
enum Kind {
    Str {
        min: Option<usize>,
        max: Option<usize>,
        max_bytes: Option<usize>,
        multiline: bool,
        pattern: Option<Pattern>,
    },
    Integer {
        min: Option<f64>,
        max: Option<f64>,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    Boolean,
    Enum(Vec<Value>),
    Object(Schema),
    Array {
        items: Box<Spec>,
        min: Option<usize>,
        max: Option<usize>,
    },
}

/// The rule of one value.
#[derive(Clone)]
pub struct Spec {
    kind: Kind,
    optional: bool,
    nullable: bool,
}

impl fmt::Debug for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            Kind::Str { .. } => "string",
            Kind::Integer { .. } => "integer",
            Kind::Number { .. } => "number",
            Kind::Boolean => "boolean",
            Kind::Enum(_) => "enum",
            Kind::Object(_) => "object",
            Kind::Array { .. } => "array",
        };
        f.debug_struct("Spec").field("type", &kind).field("optional", &self.optional).finish()
    }
}

impl Spec {
    fn of(kind: Kind) -> Spec {
        Spec { kind, optional: false, nullable: false }
    }

    /// A string (no control characters).
    pub fn string() -> Spec {
        Spec::of(Kind::Str { min: None, max: None, max_bytes: None, multiline: false, pattern: None })
    }

    /// A safe integer (`8080.0` is one).
    pub fn integer() -> Spec {
        Spec::of(Kind::Integer { min: None, max: None })
    }

    /// A finite number.
    pub fn number() -> Spec {
        Spec::of(Kind::Number { min: None, max: None })
    }

    /// `true` or `false`.
    pub fn boolean() -> Spec {
        Spec::of(Kind::Boolean)
    }

    /// One of `values` (strict equality).
    pub fn one_of(values: impl IntoIterator<Item = Value>) -> Spec {
        Spec::of(Kind::Enum(values.into_iter().collect()))
    }

    /// A nested object.
    pub fn object(schema: Schema) -> Spec {
        Spec::of(Kind::Object(schema))
    }

    /// An array of `items`.
    pub fn array(items: Spec) -> Spec {
        Spec::of(Kind::Array { items: Box::new(items), min: None, max: None })
    }

    /// The field may be absent.
    pub fn optional(mut self) -> Spec {
        self.optional = true;
        self
    }

    /// The value may be `null`.
    pub fn nullable(mut self) -> Spec {
        self.nullable = true;
        self
    }

    /// Minimum: UTF-16 units of a string, items of an array.
    pub fn min_len(mut self, n: usize) -> Spec {
        match &mut self.kind {
            Kind::Str { min, .. } | Kind::Array { min, .. } => *min = Some(n),
            _ => panic!("schema: min_len applies to strings and arrays"),
        }
        self
    }

    /// Maximum: UTF-16 units of a string, items of an array.
    pub fn max_len(mut self, n: usize) -> Spec {
        match &mut self.kind {
            Kind::Str { max, .. } | Kind::Array { max, .. } => *max = Some(n),
            _ => panic!("schema: max_len applies to strings and arrays"),
        }
        self
    }

    /// Minimum of a number.
    pub fn min(mut self, n: f64) -> Spec {
        match &mut self.kind {
            Kind::Integer { min, .. } | Kind::Number { min, .. } => *min = Some(n),
            _ => panic!("schema: min applies to numbers"),
        }
        self
    }

    /// Maximum of a number.
    pub fn max(mut self, n: f64) -> Spec {
        match &mut self.kind {
            Kind::Integer { max, .. } | Kind::Number { max, .. } => *max = Some(n),
            _ => panic!("schema: max applies to numbers"),
        }
        self
    }

    /// Maximum UTF-8 bytes of a string.
    pub fn max_bytes(mut self, n: usize) -> Spec {
        match &mut self.kind {
            Kind::Str { max_bytes, .. } => *max_bytes = Some(n),
            _ => panic!("schema: max_bytes applies to strings"),
        }
        self
    }

    /// The string may hold TAB, LF and CR.
    pub fn multiline(mut self) -> Spec {
        match &mut self.kind {
            Kind::Str { multiline, .. } => *multiline = true,
            _ => panic!("schema: multiline applies to strings"),
        }
        self
    }

    /// The string must pass `check` (else "has an invalid format").
    pub fn pattern(mut self, check: impl Fn(&str) -> bool + Send + Sync + 'static) -> Spec {
        match &mut self.kind {
            Kind::Str { pattern, .. } => *pattern = Some(Arc::new(check)),
            _ => panic!("schema: pattern applies to strings"),
        }
        self
    }
}

/// The fields of an object, in declaration order (the order of the validated output).
#[derive(Clone, Default, Debug)]
pub struct Schema {
    fields: Vec<(String, Spec)>,
}

/// A value that breaks its schema: 400 `invalid_request` with the message, plus `field` when
/// known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// The dotted and indexed path of the field (`nested.x`, `list[0]`); `None` when the body
    /// itself is not an object.
    pub field: Option<String>,
    /// The message.
    pub message: String,
}

impl Invalid {
    fn at(field: &str, message: String) -> Invalid {
        Invalid { field: Some(field.to_string()), message }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Invalid {}

impl Schema {
    /// An object with no field (only `{}` passes).
    pub fn new() -> Schema {
        Schema::default()
    }

    /// Adds a field.
    pub fn field(mut self, name: impl Into<String>, spec: Spec) -> Schema {
        self.fields.push((name.into(), spec));
        self
    }

    /// Validates `value`: the output object holds the present fields in schema order, integers
    /// normalised (`8080.0` becomes `8080`).
    pub fn validate(&self, value: &Value) -> Result<Value, Invalid> {
        validate_object(self, value, "")
    }
}

fn validate_object(schema: &Schema, value: &Value, prefix: &str) -> Result<Value, Invalid> {
    let Value::Object(map) = value else {
        return Err(if prefix.is_empty() {
            Invalid { field: None, message: "the body must be a JSON object".into() }
        } else {
            let name = &prefix[..prefix.len() - 1];
            Invalid::at(name, format!("\"{name}\" must be an object"))
        });
    };
    for k in js_keys(map) {
        if !schema.fields.iter().any(|(name, _)| name == k) {
            let path = format!("{prefix}{k}");
            return Err(Invalid::at(&path, format!("unknown field \"{path}\"")));
        }
    }
    let mut out = Map::new();
    for (name, spec) in &schema.fields {
        let path = format!("{prefix}{name}");
        match map.get(name) {
            None if spec.optional => {}
            None => return Err(Invalid::at(&path, format!("\"{path}\" is required"))),
            Some(v) => {
                out.insert(name.clone(), check_value(spec, v, &path)?);
            }
        }
    }
    Ok(Value::Object(out))
}

fn has_control(s: &str, multiline: bool) -> bool {
    s.chars().any(|c| {
        let u = c as u32;
        (u < 0x20 || u == 0x7f) && !(multiline && matches!(c, '\t' | '\n' | '\r'))
    })
}

fn check_value(spec: &Spec, v: &Value, name: &str) -> Result<Value, Invalid> {
    if v.is_null() {
        return if spec.nullable {
            Ok(Value::Null)
        } else {
            Err(Invalid::at(name, format!("\"{name}\" must not be null")))
        };
    }
    match &spec.kind {
        Kind::Str { min, max, max_bytes, multiline, pattern } => {
            let Value::String(s) = v else {
                return Err(Invalid::at(name, format!("\"{name}\" must be a string")));
            };
            let units = s.encode_utf16().count();
            if let Some(min) = min
                && units < *min
            {
                return Err(Invalid::at(
                    name,
                    format!("\"{name}\" is too short (at least {min} characters)"),
                ));
            }
            if let Some(max) = max
                && units > *max
            {
                return Err(Invalid::at(name, format!("\"{name}\" is too long (at most {max} characters)")));
            }
            if max_bytes.is_some_and(|m| s.len() > m) {
                return Err(Invalid::at(name, format!("\"{name}\" is too long")));
            }
            if has_control(s, *multiline) {
                return Err(Invalid::at(name, format!("\"{name}\" contains control characters")));
            }
            if pattern.as_ref().is_some_and(|p| !p(s)) {
                return Err(Invalid::at(name, format!("\"{name}\" has an invalid format")));
            }
            Ok(v.clone())
        }
        Kind::Integer { min, max } => {
            let Some(i) = safe_integer(v) else {
                return Err(Invalid::at(name, format!("\"{name}\" must be an integer")));
            };
            check_bounds(i as f64, *min, *max, name)?;
            Ok(Value::from(i))
        }
        Kind::Number { min, max } => {
            let Some(f) = v.as_f64().filter(|f| f.is_finite()) else {
                return Err(Invalid::at(name, format!("\"{name}\" must be a number")));
            };
            check_bounds(f, *min, *max, name)?;
            Ok(v.clone())
        }
        Kind::Boolean => match v {
            Value::Bool(_) => Ok(v.clone()),
            _ => Err(Invalid::at(name, format!("\"{name}\" must be true or false"))),
        },
        Kind::Enum(values) => {
            if values.iter().any(|x| strict_equals(x, v)) {
                Ok(v.clone())
            } else {
                let list: Vec<String> = values.iter().map(js_to_string).collect();
                Err(Invalid::at(name, format!("\"{name}\" must be one of {}", list.join(", "))))
            }
        }
        Kind::Object(schema) => validate_object(schema, v, &format!("{name}.")),
        Kind::Array { items, min, max } => {
            let Value::Array(list) = v else {
                return Err(Invalid::at(name, format!("\"{name}\" must be an array")));
            };
            if let Some(min) = min
                && list.len() < *min
            {
                return Err(Invalid::at(name, format!("\"{name}\" needs at least {min} items")));
            }
            if max.is_some_and(|m| list.len() > m) {
                return Err(Invalid::at(name, format!("\"{name}\" has too many items")));
            }
            let mut out = Vec::with_capacity(list.len());
            for (i, item) in list.iter().enumerate() {
                out.push(check_value(items, item, &format!("{name}[{i}]"))?);
            }
            Ok(Value::Array(out))
        }
    }
}

fn check_bounds(v: f64, min: Option<f64>, max: Option<f64>, name: &str) -> Result<(), Invalid> {
    if let Some(min) = min
        && v < min
    {
        return Err(Invalid::at(name, format!("\"{name}\" must be at least {}", js_number(min))));
    }
    if let Some(max) = max
        && v > max
    {
        return Err(Invalid::at(name, format!("\"{name}\" must be at most {}", js_number(max))));
    }
    Ok(())
}

/// JavaScript's `===` between two JSON values (numbers compare by value).
fn strict_equals(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(_), _) | (Value::Object(_), _) => false,
        _ => a == b,
    }
}

/// `String(v)` of a primitive, as `Array.prototype.join` writes it (`null` is empty).
fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => js_number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::Bool(b) => b.to_string(),
        Value::Array(_) | Value::Object(_) => String::from("[object]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_schema() -> Schema {
        Schema::new()
            .field(
                "name",
                Spec::string()
                    .min_len(2)
                    .max_len(8)
                    .pattern(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase())),
            )
            .field("n", Spec::integer().min(1.0).max(9.0).optional())
            .field("flag", Spec::boolean().optional())
            .field("kind", Spec::one_of([json!("a"), json!("b")]).optional())
            .field("nested", Spec::object(Schema::new().field("x", Spec::string().max_len(3))).optional())
            .field("list", Spec::array(Spec::number()).max_len(2).optional())
            .field("note", Spec::string().max_len(50).optional().multiline())
    }

    #[test]
    fn accepts_a_valid_body_in_schema_order() {
        let v = json!({"note": "two\nlines", "name": "abc", "n": 3.0, "flag": true, "kind": "b", "nested": {"x": "yz"}, "list": [1, 2.5]});
        let out = body_schema().validate(&v).expect("valid");
        assert_eq!(
            crate::http::json::stringify(&out),
            r#"{"name":"abc","n":3,"flag":true,"kind":"b","nested":{"x":"yz"},"list":[1,2.5],"note":"two\nlines"}"#
        );
    }

    #[test]
    fn names_the_field_that_breaks_it() {
        let bad = [
            (json!({"name": "abc", "extra": 1}), "extra"),
            (json!({"name": "a"}), "name"),
            (json!({"name": "abcdefghi"}), "name"),
            (json!({"name": "ABC"}), "name"),
            (json!({"name": 5}), "name"),
            (json!({"name": "abc", "n": 1.5}), "n"),
            (json!({"name": "abc", "n": 10}), "n"),
            (json!({"name": "abc", "n": "3"}), "n"),
            (json!({"name": "abc", "flag": "yes"}), "flag"),
            (json!({"name": "abc", "kind": "c"}), "kind"),
            (json!({"name": "abc", "nested": {"x": "long"}}), "nested.x"),
            (json!({"name": "abc", "nested": {"y": 1}}), "nested.y"),
            (json!({"name": "abc", "nested": []}), "nested"),
            (json!({"name": "abc", "list": [1, 2, 3]}), "list"),
            (json!({"name": "abc", "list": ["x"]}), "list[0]"),
            (json!({"name": "ab\u{0}c"}), "name"),
            (json!({"name": "abc", "note": "bell\u{7}"}), "note"),
            (json!({"name": null}), "name"),
            (json!({"__proto__": {"admin": true}, "name": "abc"}), "__proto__"),
        ];
        for (body, field) in bad {
            let e = body_schema().validate(&body).expect_err("invalid");
            assert_eq!(e.field.as_deref(), Some(field), "{body}");
        }
        assert_eq!(
            body_schema().validate(&json!([1])).unwrap_err(),
            Invalid { field: None, message: "the body must be a JSON object".into() }
        );
    }

    #[test]
    fn messages_are_verbatim() {
        let s = body_schema();
        let msg = |v: Value| s.validate(&v).unwrap_err().message;
        assert_eq!(msg(json!({})), "\"name\" is required");
        assert_eq!(msg(json!({"name": "a"})), "\"name\" is too short (at least 2 characters)");
        assert_eq!(msg(json!({"name": "abcdefghi"})), "\"name\" is too long (at most 8 characters)");
        assert_eq!(msg(json!({"name": "ABC"})), "\"name\" has an invalid format");
        assert_eq!(msg(json!({"name": 1})), "\"name\" must be a string");
        assert_eq!(msg(json!({"name": null})), "\"name\" must not be null");
        assert_eq!(msg(json!({"name": "ab\u{1}"})), "\"name\" contains control characters");
        assert_eq!(msg(json!({"name": "abc", "n": 0})), "\"n\" must be at least 1");
        assert_eq!(msg(json!({"name": "abc", "n": 10})), "\"n\" must be at most 9");
        assert_eq!(msg(json!({"name": "abc", "n": 1.5})), "\"n\" must be an integer");
        assert_eq!(msg(json!({"name": "abc", "list": [true]})), "\"list[0]\" must be a number");
        assert_eq!(msg(json!({"name": "abc", "flag": 1})), "\"flag\" must be true or false");
        assert_eq!(msg(json!({"name": "abc", "kind": "z"})), "\"kind\" must be one of a, b");
        assert_eq!(msg(json!({"name": "abc", "nested": 1})), "\"nested\" must be an object");
        assert_eq!(msg(json!({"name": "abc", "list": {}})), "\"list\" must be an array");
        assert_eq!(msg(json!({"name": "abc", "list": [1, 2, 3]})), "\"list\" has too many items");
        assert_eq!(msg(json!({"name": "abc", "nested": {"q": 1}})), "unknown field \"nested.q\"");
        let few = Schema::new().field("a", Spec::array(Spec::string()).min_len(2));
        assert_eq!(few.validate(&json!({"a": ["x"]})).unwrap_err().message, "\"a\" needs at least 2 items");
        let bytes = Schema::new().field("a", Spec::string().max_bytes(3));
        assert_eq!(bytes.validate(&json!({"a": "éé"})).unwrap_err().message, "\"a\" is too long");
    }

    #[test]
    fn lengths_count_utf16_units_and_nullable_passes_null() {
        let s = Schema::new()
            .field("e", Spec::string().max_len(2))
            .field("z", Spec::integer().nullable().optional());
        assert!(s.validate(&json!({"e": "😀"})).is_ok(), "one astral character is two units");
        assert!(s.validate(&json!({"e": "😀a"})).is_err());
        assert_eq!(s.validate(&json!({"e": "", "z": null})).expect("valid"), json!({"e": "", "z": null}));
        assert_eq!(Schema::new().validate(&json!({})).expect("valid"), json!({}));
    }

    #[test]
    fn unknown_fields_follow_object_key_order() {
        let s = Schema::new().field("a", Spec::string().optional());
        let v = crate::http::json::parse(r#"{"zz":1,"5":2}"#).expect("valid");
        assert_eq!(s.validate(&v).unwrap_err().field.as_deref(), Some("5"), "an index key comes first");
    }
}
