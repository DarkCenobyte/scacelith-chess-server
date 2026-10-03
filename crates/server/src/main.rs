//! `scacelith-server` binary. GPL-3.0-or-later.

fn main() {
    let code = scacelith_server::cli::run(std::env::args().collect());
    std::process::exit(code);
}
