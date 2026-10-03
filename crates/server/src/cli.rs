//! Command line: `scacelith-server [start|migrate|check-config|gen-secret|admin ...|version|help]`.
//! Exit codes: 0 success, 1 failure, 2 usage. See docs/RUST-PORT.md.

/// Runs the command line and returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("version") | Some("--version") | Some("-V") => {
            println!("scacelith-server {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("usage: scacelith-server [start|migrate|check-config|gen-secret|admin|version|help]");
            2
        }
    }
}
