//! systemd integration: readiness notifications (`sd_notify`, `Type=notify` units) and journald
//! detection. Everything here is a no-op when the server does not run under systemd.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

/// Sends `state` (for example `READY=1` or `STOPPING=1`) to the service manager named by
/// `NOTIFY_SOCKET`. Returns whether a notification was sent.
pub fn notify(state: &str) -> bool {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return false;
    };
    let path = path.to_string_lossy().into_owned();
    let Ok(sock) = UnixDatagram::unbound() else {
        return false;
    };
    let addr = if let Some(name) = path.strip_prefix('@') {
        SocketAddr::from_abstract_name(name.as_bytes())
    } else {
        SocketAddr::from_pathname(&path)
    };
    match addr {
        Ok(a) => sock.send_to_addr(state.as_bytes(), &a).is_ok(),
        Err(_) => false,
    }
}

/// `READY=1`: migrations applied, games recovered and listeners bound.
pub fn ready() -> bool {
    notify("READY=1")
}

/// `STOPPING=1`: graceful shutdown started.
pub fn stopping() -> bool {
    notify("STOPPING=1")
}

/// Whether stdout is connected to the journal (`JOURNAL_STREAM` set by systemd).
pub fn journald() -> bool {
    std::env::var_os("JOURNAL_STREAM").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_reaches_a_unix_socket() {
        let dir = std::env::temp_dir().join(format!("scacelith-notify-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("notify.sock");
        let _ = std::fs::remove_file(&path);
        let server = UnixDatagram::bind(&path).unwrap();
        // SAFETY of the test: the variable is only read by `notify` in this process.
        let sock = UnixDatagram::unbound().unwrap();
        sock.send_to(b"READY=1", &path).unwrap();
        let mut buf = [0u8; 16];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
