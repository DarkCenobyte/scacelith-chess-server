//! systemd integration: service notifications (`sd_notify(3)` for `Type=notify` and
//! `Type=notify-reload` units, the watchdog) and journald detection. Everything here is a no-op
//! when the server does not run under systemd.

use std::os::fd::AsFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

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

/// `RELOADING=1` with the current `MONOTONIC_USEC` (`Type=notify-reload`): the certificates are
/// being reloaded. Send [`ready`] once done.
pub fn reloading() -> bool {
    notify(&format!("RELOADING=1\nMONOTONIC_USEC={}", crate::sys::monotonic_usec()))
}

/// `STATUS=...`: a one-line status shown by `systemctl status`.
pub fn status(text: &str) -> bool {
    let line: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    notify(&format!("STATUS={line}"))
}

/// `WATCHDOG=1`: keep-alive ping for `WatchdogSec=`.
pub fn watchdog() -> bool {
    notify("WATCHDOG=1")
}

/// The interval at which [`watchdog`] must be called (half the `WatchdogSec=` the manager set),
/// or `None` when the watchdog is off or meant for another process (`WATCHDOG_PID`).
pub fn watchdog_interval() -> Option<Duration> {
    if let Some(pid) = std::env::var_os("WATCHDOG_PID")
        && pid.to_str()?.parse::<u32>().ok()? != std::process::id()
    {
        return None;
    }
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if usec == 0 { None } else { Some(Duration::from_micros(usec / 2)) }
}

/// Whether stdout is connected to the journal: `JOURNAL_STREAM` (`<device>:<inode>`, set by
/// systemd) names the stream that stdout is, not one that a wrapper script redirected.
pub fn journald() -> bool {
    stream_is_journal(&std::io::stdout())
}

/// The same as [`journald`] for stderr (administration commands log there).
pub fn journald_stderr() -> bool {
    stream_is_journal(&std::io::stderr())
}

fn stream_is_journal(stream: &impl AsFd) -> bool {
    match std::env::var_os("JOURNAL_STREAM") {
        Some(value) => value.to_str().is_some_and(|v| stream_matches(v, stream)),
        None => false,
    }
}

/// Whether `stream` is the file named by a `JOURNAL_STREAM` value (`<device>:<inode>`).
fn stream_matches(value: &str, stream: &impl AsFd) -> bool {
    let Some((dev, ino)) = value.split_once(':') else {
        return false;
    };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
        return false;
    };
    let Ok(fd) = stream.as_fd().try_clone_to_owned() else {
        return false;
    };
    match std::fs::File::from(fd).metadata() {
        Ok(m) => m.dev() == dev && m.ino() == ino,
        Err(_) => false,
    }
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
        let sock = UnixDatagram::unbound().unwrap();
        sock.send_to(b"READY=1", &path).unwrap();
        let mut buf = [0u8; 16];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stream_matches_its_own_device_and_inode() {
        let dir = std::env::temp_dir().join(format!("scacelith-journal-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = std::fs::File::create(dir.join("stream")).unwrap();
        let m = file.metadata().unwrap();
        assert!(stream_matches(&format!("{}:{}", m.dev(), m.ino()), &file));
        assert!(!stream_matches(&format!("{}:{}", m.dev(), m.ino() + 1), &file));
        assert!(!stream_matches("garbage", &file));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
