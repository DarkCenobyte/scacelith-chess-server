//! protogen: reads `protocol/scacelith-v1.json`, validates it (and the append-only rule against
//! the frozen manifests of `protocol/frozen/`), and writes every derived file:
//!
//! | File | Content |
//! |---|---|
//! | `crates/protocol/src/gen.rs` | the Rust codec |
//! | `crates/protocol/src/gen_json.rs` | the JSON form of the messages (tests) |
//! | `docs/PROTOCOL.md` | the generated tables (between `protogen` markers) |
//! | `test/fixtures/protocol-vectors.json` | the golden vectors |
//! | `../src/net/protocol_gen.h`, `.cpp` | the game client's C++ codec |
//!
//! Paths are relative to `dedicated-server/`. The library compiles this module for its tests
//! (the freshness check) and with the `gen` feature (the binary).

mod canon;
mod cpp;
mod docs;
pub mod interp;
mod manifest;
pub mod model;
mod rust;
mod text;
mod vectors;

use std::fs;
use std::path::{Path, PathBuf};

pub use model::Schema;

/// Path of the schema, relative to the root.
pub const SCHEMA_PATH: &str = "protocol/scacelith-v1.json";
/// Directory of the frozen manifests, relative to the root.
pub const FROZEN_DIR: &str = "protocol/frozen";
const DOC_PATH: &str = "docs/PROTOCOL.md";

/// The `dedicated-server/` directory of the source tree this crate was built from.
pub fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// What a run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Writes the derived files that changed.
    Write,
    /// Writes nothing; fails when a derived file is stale.
    Check,
    /// Writes the manifest of the current minor (`force`: replaces a different one).
    Freeze { force: bool },
}

/// The schema of a root, parsed and validated.
pub fn load(root: &Path) -> Result<Schema, String> {
    let path = root.join(SCHEMA_PATH);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Schema::parse(&text).map_err(|errors| format!("{SCHEMA_PATH}:\n  {}", errors.join("\n  ")))
}

/// The frozen manifests of a root, as `(file name, schema)`.
fn frozen(root: &Path) -> Result<Vec<(String, Schema)>, String> {
    let dir = root.join(FROZEN_DIR);
    let Ok(entries) = fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_some_and(|x| x == "json") {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let schema = manifest::parse(&name, &text)?;
            if name != manifest::file_name(schema.protocol, schema.minor) {
                return Err(format!("{FROZEN_DIR}/{name}: holds v{}.{}", schema.protocol, schema.minor));
            }
            out.push((name, schema));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Every derived file: `(path relative to the root, content)`.
pub fn outputs(root: &Path, schema: &Schema) -> Result<Vec<(&'static str, String)>, String> {
    let doc_path = root.join(DOC_PATH);
    let doc = fs::read_to_string(&doc_path).map_err(|e| format!("{}: {e}", doc_path.display()))?;
    Ok(vec![
        ("crates/protocol/src/gen.rs", rust::codec(schema)),
        ("crates/protocol/src/gen_json.rs", rust::json_bridge(schema)),
        (DOC_PATH, docs::splice(&doc, schema)?),
        ("test/fixtures/protocol-vectors.json", vectors::build(schema)),
        ("../src/net/protocol_gen.h", cpp::header(schema)),
        ("../src/net/protocol_gen.cpp", cpp::source(schema)),
    ])
}

/// Runs protogen on a root; `Ok` carries the report lines, `Err` the reason of the failure.
pub fn run(root: &Path, mode: Mode) -> Result<Vec<String>, String> {
    let schema = load(root)?;
    let frozen = frozen(root)?;
    let schemas: Vec<Schema> = frozen.iter().map(|(_, s)| s.clone()).collect();
    let violations = manifest::check(&schema, &schemas);
    if !violations.is_empty() {
        return Err(format!("append-only rule:\n  {}", violations.join("\n  ")));
    }
    let mut report = Vec::new();
    if let Mode::Freeze { force } = mode {
        let name = manifest::file_name(schema.protocol, schema.minor);
        let path = root.join(FROZEN_DIR).join(&name);
        let text = manifest::render(&schema);
        match fs::read_to_string(&path) {
            Ok(current) if current == text => report.push(format!("{FROZEN_DIR}/{name} up to date")),
            Ok(_) if !force => {
                return Err(format!("{FROZEN_DIR}/{name} exists and differs (--force replaces it)"));
            }
            _ => {
                fs::create_dir_all(root.join(FROZEN_DIR)).map_err(|e| format!("{FROZEN_DIR}: {e}"))?;
                fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                report.push(format!("wrote {FROZEN_DIR}/{name}"));
            }
        }
        return Ok(report);
    }
    if !frozen.iter().any(|(_, s)| s.protocol == schema.protocol && s.minor == schema.minor) {
        report.push(format!(
            "note: v{}.{} is not frozen yet (protogen --freeze when it is released)",
            schema.protocol, schema.minor
        ));
    }
    let files = outputs(root, &schema)?;
    let total = files.len();
    let mut stale = Vec::new();
    for (rel, content) in files {
        let path = root.join(rel);
        if fs::read_to_string(&path).is_ok_and(|current| current == content) {
            continue;
        }
        match mode {
            Mode::Check => stale.push(rel),
            _ => {
                fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
                report.push(format!("wrote {rel}"));
            }
        }
    }
    if !stale.is_empty() {
        return Err(format!(
            "stale generated files (run `cargo run -p scacelith-protocol --features gen --bin protogen`):\n  {}",
            stale.join("\n  ")
        ));
    }
    if report.iter().all(|l| l.starts_with("note:")) {
        report.push(format!("{total} files up to date (fingerprint {:#010x})", schema.fingerprint));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed derived files match the schema, and the schema keeps the frozen wire.
    #[test]
    fn generated_files_are_fresh() {
        if let Err(e) = run(&default_root(), Mode::Check) {
            panic!("{e}");
        }
    }
}
