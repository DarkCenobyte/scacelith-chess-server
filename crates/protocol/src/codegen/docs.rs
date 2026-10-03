//! Markdown emitter: the reference tables of `docs/PROTOCOL.md`. The prose is written by hand;
//! each table lives between `<!-- protogen:begin NAME -->` and `<!-- protogen:end NAME -->`
//! and is replaced on every run.

use std::fmt::Write as _;

use super::model::{Dir, Field, Message, Schema, Type};

/// Names of the generated sections, in the order they are expected in the document.
pub const SECTIONS: [&str; 8] =
    ["summary", "ranges", "close-codes", "messages", "fields", "structs", "enums", "flags"];

/// `doc` with its generated sections replaced; every section must appear exactly once.
pub fn splice(doc: &str, schema: &Schema) -> Result<String, String> {
    let mut out = String::with_capacity(doc.len() + 4096);
    let mut rest = doc;
    for name in SECTIONS {
        let begin = format!("<!-- protogen:begin {name} -->");
        let end = format!("<!-- protogen:end {name} -->");
        let (Some(b), Some(e)) = (rest.find(&begin), rest.find(&end)) else {
            return Err(format!("docs/PROTOCOL.md: section {name} is missing or out of order"));
        };
        if e < b || doc.matches(&begin).count() != 1 || doc.matches(&end).count() != 1 {
            return Err(format!("docs/PROTOCOL.md: section {name} must appear once, begin before end"));
        }
        out.push_str(&rest[..b + begin.len()]);
        out.push_str("\n\n");
        out.push_str(section(name, schema).trim_end());
        out.push_str("\n\n");
        out.push_str(&end);
        rest = &rest[e + end.len()..];
    }
    out.push_str(rest);
    Ok(out)
}

fn section(name: &str, schema: &Schema) -> String {
    match name {
        "summary" => summary(schema),
        "ranges" => ranges(schema),
        "messages" => messages(schema),
        "fields" => fields(schema),
        "structs" => structs(schema),
        "enums" => enums(schema),
        "flags" => flags(schema),
        "close-codes" => close_codes(schema),
        _ => unreachable!("listed in SECTIONS"),
    }
}

/// A table cell: one line, `|` escaped.
fn cell(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|")
}

/// First sentence of a description.
fn first_sentence(text: &str) -> String {
    let text = cell(text);
    match text.find(". ") {
        Some(i) => text[..=i].to_owned(),
        None => text,
    }
}

fn type_label(ty: &Type) -> String {
    match ty {
        Type::Enum(name) => format!("enum [{name}](#{})", name.to_ascii_lowercase()),
        Type::Struct(name) => format!("[{name}](#{})", name.to_ascii_lowercase()),
        Type::List(item) => format!("list16 of {}", type_label(item)),
        other => format!("`{}`", other.spelling()),
    }
}

fn bounds_label(f: &Field) -> String {
    match &f.ty {
        Type::Str8 => format!("{}..{} bytes", f.lo(), f.hi()),
        Type::List(_) => format!("at most {} items", f.hi()),
        Type::Id53 => "< 2^53".into(),
        Type::F64 => "finite".into(),
        _ if f.is_bounded() => format!("{}..{}", f.lo(), f.hi()),
        _ => String::new(),
    }
}

fn summary(schema: &Schema) -> String {
    let count = |dir| schema.messages.iter().filter(|m| m.dir == dir).count();
    let mut o = String::from("| | |\n|---|---|\n");
    let _ = writeln!(o, "| Protocol version (`proto`) | {} |", schema.protocol);
    let _ = writeln!(o, "| Minor version (`minor`) | {} |", schema.minor);
    let _ = writeln!(
        o,
        "| Capability bits (`caps`) | {} |",
        if schema.caps.is_empty() {
            "none defined".to_owned()
        } else {
            schema.caps.iter().map(|c| format!("`{}` (bit {})", c.name, c.bit)).collect::<Vec<_>>().join(", ")
        }
    );
    let _ = writeln!(o, "| WebSocket subprotocol | `{}` |", schema.subprotocol);
    let _ = writeln!(o, "| Schema fingerprint | `{:#010x}` (informational) |", schema.fingerprint);
    for c in &schema.constants {
        let _ = writeln!(o, "| `{}` | {} |", c.name, c.value);
    }
    let _ = writeln!(
        o,
        "| Messages | {} client to server, {} server to client |",
        count(Dir::C2s),
        count(Dir::S2c)
    );
    o
}

fn ranges(schema: &Schema) -> String {
    let mut o = String::from("| Type bytes | Direction | Area | Status |\n|---|---|---|---|\n");
    for r in &schema.ranges {
        let used = schema.messages.iter().filter(|m| (r.first..=r.last).contains(&m.id)).count();
        let status = match (r.published, used) {
            (false, _) => "never published: private experiments, refused by every published peer".to_owned(),
            (true, 0) => "reserved for later minors".to_owned(),
            (true, n) => format!("{n} assigned"),
        };
        let _ = writeln!(
            o,
            "| `{:#04X}`-`{:#04X}` | {} | {} | {status} |",
            r.first,
            r.last,
            r.dir.as_str(),
            r.area
        );
    }
    o.replace("0X", "0x")
}

fn messages(schema: &Schema) -> String {
    let mut o = String::from("| Type | Message | Dir | Size (bytes) | Summary |\n|---|---|---|---|---|\n");
    for m in &schema.messages {
        let (min, max) = schema.message_size(m);
        let size = if min == max { min.to_string() } else { format!("{min}..{max}") };
        let _ = writeln!(
            o,
            "| `{}` | [{}](#{}) | {} | {size} | {} |",
            format!("{:#04X}", m.id).replace("0X", "0x"),
            m.key,
            anchor(m),
            m.dir.as_str(),
            first_sentence(&m.doc)
        );
    }
    o
}

/// Anchor of the heading of a message in the fields section.
fn anchor(m: &Message) -> String {
    format!("{:02x}-{}", m.id, m.key.to_ascii_lowercase().replace('_', "-"))
}

fn fields(schema: &Schema) -> String {
    let mut o = String::new();
    for m in &schema.messages {
        let (min, max) = schema.message_size(m);
        let size = if min == max { format!("{min} bytes") } else { format!("{min} to {max} bytes") };
        let _ = writeln!(o, "<a id=\"{}\"></a>", anchor(m));
        let _ = writeln!(
            o,
            "#### `{}` {} ({}, {size})\n",
            format!("{:#04X}", m.id).replace("0X", "0x"),
            m.key,
            m.dir.as_str()
        );
        if !m.doc.is_empty() {
            let _ = writeln!(o, "{}\n", m.doc.trim());
        }
        if let Some(of) = &m.relay_of {
            let _ = writeln!(
                o,
                "The relay of the client `{of}`: the same fields without `seq`, copied byte for byte.\n"
            );
        }
        o.push_str(&field_table(schema, &m.fields, Some(1)));
        o.push('\n');
    }
    o
}

/// Table of a field list; `start` is the offset of the first field when known.
fn field_table(schema: &Schema, fields: &[Field], start: Option<usize>) -> String {
    let mut o = String::from("| Offset | Field | Type | Bounds | Meaning |\n|---|---|---|---|---|\n");
    let mut offset = start;
    for f in fields {
        let at = offset.map_or_else(|| "...".to_owned(), |n| n.to_string());
        let _ = writeln!(
            o,
            "| {at} | `{}` | {} | {} | {} |",
            f.name,
            type_label(&f.ty),
            bounds_label(f),
            cell(f.description())
        );
        let (lo, hi) = schema.type_size(f);
        offset = offset.filter(|_| lo == hi).map(|n| n + lo);
    }
    o
}

fn structs(schema: &Schema) -> String {
    let mut o = String::new();
    for s in &schema.structs {
        let (min, max) = schema.fields_size(&s.fields);
        let size = if min == max { format!("{min} bytes") } else { format!("{min} to {max} bytes") };
        let _ = writeln!(o, "#### {}\n", s.name);
        let _ = writeln!(o, "{} ({size}, inline.)\n", s.doc.trim().trim_end_matches('.'));
        o.push_str(&field_table(schema, &s.fields, Some(0)));
        o.push('\n');
    }
    o
}

fn enums(schema: &Schema) -> String {
    let mut o = String::new();
    for e in &schema.enums {
        let _ = writeln!(o, "#### {}\n", e.name);
        let kind = if e.open {
            format!(
                "Open: a later minor may add values. A client keeps an unknown value and {}",
                if e.unknown_doc.is_empty() {
                    "ignores it.".to_owned()
                } else {
                    lower_first(e.unknown_doc.trim())
                }
            )
        } else {
            "Closed: the values never change within protocol 1.".to_owned()
        };
        let _ = writeln!(o, "{} {kind}\n", e.doc.trim());
        o.push_str("| Value | Name | Meaning |\n|---|---|---|\n");
        let mut rows: Vec<(u8, String, String)> =
            e.values.iter().map(|v| (v.value, format!("`{}`", v.name), cell(&v.doc))).collect();
        rows.extend(
            e.reserved
                .iter()
                .map(|v| (v.value, format!("~~`{}`~~", v.name), format!("Reserved: {}", cell(&v.doc)))),
        );
        rows.sort_by_key(|r| r.0);
        for (value, name, doc) in rows {
            let _ = writeln!(o, "| {value} | {name} | {doc} |");
        }
        o.push('\n');
    }
    o
}

fn lower_first(text: &str) -> String {
    let mut c = text.chars();
    c.next().map_or_else(String::new, |first| first.to_lowercase().collect::<String>() + c.as_str())
}

fn flags(schema: &Schema) -> String {
    let mut o = String::new();
    for f in &schema.flags {
        let _ = writeln!(o, "#### {}\n", f.name);
        let _ = writeln!(o, "{}\n", f.doc.trim());
        o.push_str("| Bit | Name | Meaning |\n|---|---|---|\n");
        for b in &f.bits {
            let _ = writeln!(o, "| `{:#04x}` | `{}` | {} |", b.value, b.name, cell(&b.doc));
        }
        o.push('\n');
    }
    o
}

fn close_codes(schema: &Schema) -> String {
    let mut o = String::from("| Code | Name | After | Meaning |\n|---|---|---|---|\n");
    for c in &schema.close_codes {
        let after = match &c.error {
            Some(e) => {
                format!("fatal `Error` {e} ({})", schema.enum_("ErrorCode").by_name(e).map_or(0, |v| v.value))
            }
            None if c.code >= 4000 => "no `Error`".to_owned(),
            None => "WebSocket".to_owned(),
        };
        let _ = writeln!(o, "| {} | `{}` | {after} | {} |", c.code, c.name, cell(&c.doc));
    }
    o
}
