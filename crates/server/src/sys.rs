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

/// Raises the soft limit of open files to the hard limit and returns the new soft limit.
pub fn raise_nofile_limit() -> io::Result<u64> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid, writable rlimit structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if lim.rlim_cur < lim.rlim_max {
        let want = libc::rlimit { rlim_cur: lim.rlim_max, rlim_max: lim.rlim_max };
        // SAFETY: `want` is a valid rlimit structure that does not exceed the hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &want) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lim = want;
    }
    Ok(lim.rlim_cur)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_work() {
        std::thread::spawn(|| set_current_thread_nice(19).unwrap()).join().unwrap();
        assert!(raise_nofile_limit().unwrap() > 0);
        assert!(process_cpu_seconds() >= 0.0);
    }
}
