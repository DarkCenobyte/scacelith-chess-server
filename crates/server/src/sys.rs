//! Thin safe wrappers over the few libc calls the server needs. This is the only module allowed
//! to use `unsafe`; every block states why it is sound.
#![allow(unsafe_code)]

use std::io;

/// Lowers the scheduling priority of the calling thread to `nice` (0..19). Used by the GIF
/// render threads and the analysis threads so they never compete with games.
pub fn set_current_thread_nice(nice: i32) -> io::Result<()> {
    // SAFETY: gettid has no preconditions and returns the caller's thread id.
    let tid = unsafe { libc::gettid() };
    set_priority(libc::PRIO_PROCESS, tid as libc::id_t, nice)
}

/// Lowers the scheduling priority of another process (for example a Stockfish child).
pub fn set_process_nice(pid: u32, nice: i32) -> io::Result<()> {
    set_priority(libc::PRIO_PROCESS, pid as libc::id_t, nice)
}

fn set_priority(which: libc::__priority_which_t, who: libc::id_t, nice: i32) -> io::Result<()> {
    // SAFETY: setpriority only reads its integer arguments; on Linux PRIO_PROCESS with a thread
    // id applies to that thread alone.
    let rc = unsafe { libc::setpriority(which, who, nice) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The soft and hard limits of open files (`RLIMIT_NOFILE`).
pub fn nofile_limit() -> io::Result<(u64, u64)> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid, writable rlimit structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((lim.rlim_cur, lim.rlim_max))
}

/// Raises the soft limit of open files to the hard limit and returns the new soft limit.
pub fn raise_nofile_limit() -> io::Result<u64> {
    let (soft, hard) = nofile_limit()?;
    if soft < hard {
        let want = libc::rlimit { rlim_cur: hard, rlim_max: hard };
        // SAFETY: `want` is a valid rlimit structure that does not exceed the hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &want) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(hard);
    }
    Ok(soft)
}

/// CPU time (user + system) consumed by the process, in seconds.
pub fn process_cpu_seconds() -> f64 {
    // SAFETY: an all-zero rusage is a valid value for getrusage to overwrite.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable rusage structure.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0.0;
    }
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// The host name (`gethostname`), `localhost` when it cannot be read. Default `INSTANCE_ID` and
/// SMTP `EHLO` name.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the pointer and length describe `buf`, which gethostname may fill entirely; the
    // result is read only up to the first NUL (or the whole buffer when truncated).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    if name.is_empty() { "localhost".to_string() } else { name }
}

/// Size of a memory page in bytes (4096 when unknown).
pub fn page_size() -> u64 {
    // SAFETY: sysconf only reads its integer argument.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

/// Clock ticks per second of the `/proc` CPU times (100 when unknown).
pub fn clock_ticks_per_second() -> u64 {
    // SAFETY: sysconf only reads its integer argument.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

/// `CLOCK_MONOTONIC` in microseconds, the clock of systemd's `MONOTONIC_USEC`.
pub fn monotonic_usec() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC always exists on Linux.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_work() {
        std::thread::spawn(|| set_current_thread_nice(19).unwrap()).join().unwrap();
        assert!(raise_nofile_limit().unwrap() > 0);
        let (soft, hard) = nofile_limit().unwrap();
        assert!(soft > 0 && soft <= hard);
        assert!(process_cpu_seconds() >= 0.0);
        assert!(!hostname().is_empty());
        assert!(page_size() >= 4096);
        assert!(clock_ticks_per_second() > 0);
        let a = monotonic_usec();
        assert!(a > 0 && monotonic_usec() >= a);
    }
}
