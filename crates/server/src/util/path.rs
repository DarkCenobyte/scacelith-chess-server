//! Lexical path resolution, as Node's `path.resolve` and `path.join` (no file system access and
//! no symbolic link resolution, unlike `fs::canonicalize`).

use std::path::{Component, Path, PathBuf};

/// `path.resolve(base, p)`: `p` if absolute, else `base/p`, then normalised (`.` removed, `..`
/// applied, repeated and trailing separators dropped). `base` should be absolute (the working
/// directory).
pub fn resolve(base: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { normalize(path) } else { normalize(&base.join(path)) }
}

/// `path.join(base, p)` for an absolute `base`: `base/p` normalised.
pub fn join(base: &Path, p: &str) -> PathBuf {
    normalize(&base.join(p))
}

/// Lexical normalisation of `p`: `.` components removed and `..` applied (never above the root of
/// an absolute path).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

/// A path as text (lossy for names that are not UTF-8).
pub fn to_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_like_node() {
        let cwd = Path::new("/srv/scacelith");
        assert_eq!(resolve(cwd, "./data"), Path::new("/srv/scacelith/data"));
        assert_eq!(resolve(cwd, "data/"), Path::new("/srv/scacelith/data"));
        assert_eq!(resolve(cwd, "../x//y/./z"), Path::new("/srv/x/y/z"));
        assert_eq!(resolve(cwd, "/var/lib/scacelith/"), Path::new("/var/lib/scacelith"));
        assert_eq!(resolve(cwd, "/../../etc"), Path::new("/etc"));
        assert_eq!(join(Path::new("/srv/data"), "scacelith.db"), Path::new("/srv/data/scacelith.db"));
        assert_eq!(normalize(Path::new("a/../..")), Path::new(".."));
    }
}
