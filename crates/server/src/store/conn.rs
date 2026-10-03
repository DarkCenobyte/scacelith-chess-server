//! Opening a connection with the store's pragmas.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

use super::error::Result;

/// How long a statement waits for another connection's lock before failing with `busy`.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Prepared statements kept per connection (the store has about 150 distinct statements).
const STATEMENT_CACHE: usize = 256;

/// Where the database lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DbPath {
    /// A file (WAL mode, shared by every connection of the store).
    File(PathBuf),
    /// A private in-memory database (tests): one connection, the writer's.
    Memory,
}

impl DbPath {
    /// `""` and `":memory:"` (also as a file name) mean an in-memory database.
    pub(crate) fn parse(path: &str) -> DbPath {
        let p = Path::new(path);
        if path.is_empty() || path == ":memory:" || p.file_name().is_some_and(|n| n == ":memory:") {
            DbPath::Memory
        } else {
            DbPath::File(p.to_path_buf())
        }
    }

    /// The path as text (`:memory:` for an in-memory database).
    pub(crate) fn display(&self) -> String {
        match self {
            DbPath::File(p) => p.display().to_string(),
            DbPath::Memory => ":memory:".into(),
        }
    }
}

/// The role of a connection, which decides its open flags and pragmas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The writer: creates the file, sets WAL, `synchronous=FULL` and `secure_delete`.
    Writer,
    /// A reader of a writable store (`query_only`).
    Reader,
    /// A connection of a read-only store (opened read-only; the file must exist).
    ReadOnly,
}

/// Memory settings of the connections (`DB_CACHE_MB`, `DB_MMAP_MB`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tuning {
    pub cache_mb: i64,
    pub mmap_mb: i64,
}

/// Opens a connection and applies the pragmas of its role.
pub(crate) fn open(path: &DbPath, role: Role, tuning: Tuning) -> Result<Connection> {
    let conn = match path {
        DbPath::Memory => Connection::open_in_memory()?,
        DbPath::File(file) => {
            let flags = match role {
                Role::Writer => {
                    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
                        std::fs::create_dir_all(dir).map_err(|e| {
                            super::StoreError::new(
                                super::ErrorKind::Sqlite,
                                format!("cannot create {}: {e}", dir.display()),
                            )
                        })?;
                    }
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                }
                Role::Reader => OpenFlags::SQLITE_OPEN_READ_WRITE,
                Role::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            };
            Connection::open_with_flags(file, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?
        }
    };
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
    // Double-quoted strings are identifiers only: a typo never turns into a string literal.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if role == Role::Writer {
        if matches!(path, DbPath::File(_)) {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
        // Deleted rows are overwritten: erased addresses and deleted accounts leave no trace in
        // the file (DESIGN 7).
        conn.pragma_update(None, "secure_delete", "ON")?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -1024 * tuning.cache_mb.max(2))?;
    if matches!(path, DbPath::File(_)) {
        conn.pragma_update(None, "mmap_size", 1024 * 1024 * tuning.mmap_mb.max(0))?;
    }
    if role == Role::Reader {
        conn.pragma_update(None, "query_only", "ON")?;
    }
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_paths() {
        assert_eq!(DbPath::parse(""), DbPath::Memory);
        assert_eq!(DbPath::parse(":memory:"), DbPath::Memory);
        assert_eq!(DbPath::parse("/tmp/x/:memory:"), DbPath::Memory);
        assert_eq!(DbPath::parse("data/scacelith.db"), DbPath::File("data/scacelith.db".into()));
    }
}
