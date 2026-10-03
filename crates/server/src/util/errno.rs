//! Symbolic names of system error numbers (`ENOENT`, `EACCES`...), the `code` that Node put on
//! its I/O errors and that messages and logs show (`cannot read /etc/x.env (ENOENT)`).

use std::io;

/// The symbolic name of an `errno` value, for the errors the server can meet.
pub fn errno_name(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::ESRCH => "ESRCH",
        libc::EINTR => "EINTR",
        libc::EIO => "EIO",
        libc::ENXIO => "ENXIO",
        libc::E2BIG => "E2BIG",
        libc::ENOEXEC => "ENOEXEC",
        libc::EBADF => "EBADF",
        libc::ECHILD => "ECHILD",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EBUSY => "EBUSY",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENODEV => "ENODEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EISDIR => "EISDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ETXTBSY => "ETXTBSY",
        libc::EFBIG => "EFBIG",
        libc::ENOSPC => "ENOSPC",
        libc::ESPIPE => "ESPIPE",
        libc::EROFS => "EROFS",
        libc::EMLINK => "EMLINK",
        libc::EPIPE => "EPIPE",
        libc::ERANGE => "ERANGE",
        libc::EDEADLK => "EDEADLK",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENOSYS => "ENOSYS",
        libc::ENOTEMPTY => "ENOTEMPTY",
        libc::ELOOP => "ELOOP",
        libc::EDQUOT => "EDQUOT",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::EADDRINUSE => "EADDRINUSE",
        libc::EADDRNOTAVAIL => "EADDRNOTAVAIL",
        libc::ENETDOWN => "ENETDOWN",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::ECONNABORTED => "ECONNABORTED",
        libc::ECONNRESET => "ECONNRESET",
        libc::ENOBUFS => "ENOBUFS",
        libc::ENOTCONN => "ENOTCONN",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::EHOSTUNREACH => "EHOSTUNREACH",
        libc::EALREADY => "EALREADY",
        libc::EINPROGRESS => "EINPROGRESS",
        libc::EPROTO => "EPROTO",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::EILSEQ => "EILSEQ",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ECANCELED => "ECANCELED",
        _ => return None,
    })
}

/// What Node printed for a failed operation (`e.code || e.message`): the errno name when the
/// error comes from the system, else the error's message.
pub fn io_error_code(e: &io::Error) -> String {
    match e.raw_os_error().and_then(errno_name) {
        Some(name) => name.to_string(),
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let missing = std::fs::read("/nonexistent/scacelith/x.env").unwrap_err();
        assert_eq!(io_error_code(&missing), "ENOENT");
        assert_eq!(errno_name(libc::EACCES), Some("EACCES"));
        assert_eq!(errno_name(-1), None);
        assert_eq!(io_error_code(&io::Error::other("boom")), "boom");
    }
}
