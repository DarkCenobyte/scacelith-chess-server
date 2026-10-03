//! protogen: the code generator of the realtime protocol.
//!
//! ```text
//! protogen                 validate protocol/scacelith-v1.json, write the derived files that changed
//! protogen --check         write nothing; exit 1 when a derived file is stale or a rule is broken
//! protogen --freeze        write protocol/frozen/v{proto}.{minor}.json (a released minor)
//! protogen --freeze --force  replace a frozen manifest (only before the minor is released)
//! protogen --root DIR      use DIR as the dedicated-server directory
//! ```
//!
//! Run it from anywhere with `cargo run -p scacelith-protocol --features gen --bin protogen`.

use std::path::PathBuf;
use std::process::ExitCode;

use scacelith_protocol::codegen::{self, Mode};

const USAGE: &str = "usage: protogen [--check | --freeze [--force]] [--root DIR]";

fn main() -> ExitCode {
    let mut mode = Mode::Write;
    let mut force = false;
    let mut root: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => mode = Mode::Check,
            "--freeze" => mode = Mode::Freeze { force: false },
            "--force" => force = true,
            "--root" => match args.next() {
                Some(dir) => root = Some(PathBuf::from(dir)),
                None => return usage(),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => return usage(),
        }
    }
    match (mode, force) {
        (Mode::Freeze { .. }, _) => mode = Mode::Freeze { force },
        (_, true) => return usage(),
        _ => {}
    }
    let root = root.unwrap_or_else(codegen::default_root);
    match codegen::run(&root, mode) {
        Ok(report) => {
            for line in report {
                println!("protogen: {line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("protogen: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
