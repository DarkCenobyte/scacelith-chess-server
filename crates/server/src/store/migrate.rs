//! Schema migrations: numbered SQL files embedded in the binary, applied in order, each in its own
//! `BEGIN IMMEDIATE` transaction, and recorded with a checksum in `schema_migrations`.
//!
//! Before anything is applied, every recorded migration is verified: one this server does not
//! know (a newer server wrote it) or whose file changed fails the start. A migration missing below
//! the latest applied one is applied (gaps are filled), so does a migration applied meanwhile by
//! another process (it is verified and skipped).

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::error::{ErrorKind, Result, StoreError};

/// One migration.
#[derive(Debug, Clone)]
pub struct Migration {
    /// Version (the number of the file name).
    pub version: i64,
    /// Name (`NNN_name`, the file name without `.sql`).
    pub name: String,
    /// The SQL text (several statements).
    pub sql: String,
}

impl Migration {
    /// A migration from its version, name and SQL text.
    pub fn new(version: i64, name: impl Into<String>, sql: impl Into<String>) -> Migration {
        Migration { version, name: name.into(), sql: sql.into() }
    }

    /// SHA-256 (lowercase hex) of the SQL text with CRLF line ends normalized to LF.
    pub fn checksum(&self) -> String {
        hex::encode(Sha256::digest(self.sql.replace("\r\n", "\n").as_bytes()))
    }
}

/// The migrations of this server, in version order.
pub fn embedded() -> Vec<Migration> {
    vec![Migration::new(1, "001_initial", include_str!("../../migrations/001_initial.sql"))]
}

/// What a migration run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Versions applied by this run, in order.
    pub applied: Vec<i64>,
    /// The latest version known to this server (0 without migrations).
    pub version: i64,
}

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    applied_at INTEGER NOT NULL,
    checksum   TEXT    NOT NULL
) STRICT";

/// Applies `migrations` (sorted by version, versions unique) on `conn`, outside any transaction.
/// `now` is the `applied_at` of every migration of the run.
pub(crate) fn run(conn: &Connection, migrations: &[Migration], now: i64) -> Result<MigrationReport> {
    let mut sorted: Vec<&Migration> = migrations.iter().collect();
    sorted.sort_by_key(|m| m.version);
    if let Some(w) = sorted.windows(2).find(|w| w[0].version == w[1].version) {
        return Err(StoreError::new(
            ErrorKind::MigrationFailed,
            format!("two migrations have version {}: {} and {}", w[0].version, w[0].name, w[1].name),
        ));
    }
    conn.execute_batch(CREATE_TABLE)?;

    let applied: Vec<(i64, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (version, name, checksum) in &applied {
        let Some(m) = sorted.iter().find(|m| m.version == *version) else {
            return Err(StoreError::new(
                ErrorKind::MigrationMissing,
                format!(
                    "database has migration {name}, unknown to this server (database from a newer server?)"
                ),
            ));
        };
        verify(m, checksum)?;
    }

    let mut report = MigrationReport { applied: Vec::new(), version: sorted.last().map_or(0, |m| m.version) };
    for m in sorted {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match apply(conn, m, now) {
            Ok(done) => {
                if let Err(e) = conn.execute_batch("COMMIT") {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                if done {
                    report.applied.push(m.version);
                }
            }
            Err(e) => {
                if !conn.is_autocommit() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                return Err(e);
            }
        }
    }

    if conn
        .query_row("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta'", [], |_| Ok(()))
        .optional()?
        .is_some()
    {
        conn.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('server_id', ?1)", [uuid_v4()?])?;
    }
    Ok(report)
}

/// Applies one migration inside the open transaction; `false` when another process applied it
/// meanwhile.
fn apply(conn: &Connection, m: &Migration, now: i64) -> Result<bool> {
    let existing: Option<String> = conn
        .query_row("SELECT checksum FROM schema_migrations WHERE version = ?1", [m.version], |r| r.get(0))
        .optional()?;
    if let Some(checksum) = existing {
        verify(m, &checksum)?;
        return Ok(false);
    }
    conn.execute_batch(&m.sql).map_err(|e| {
        StoreError::new(ErrorKind::MigrationFailed, format!("migration {} failed: {e}", m.name))
    })?;
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at, checksum) VALUES (?1, ?2, ?3, ?4)",
        params![m.version, m.name, now, m.checksum()],
    )?;
    Ok(true)
}

fn verify(m: &Migration, checksum: &str) -> Result<()> {
    if m.checksum() != checksum {
        return Err(StoreError::new(
            ErrorKind::MigrationChecksum,
            format!("migration {} was modified after it was applied (checksum mismatch)", m.name),
        ));
    }
    Ok(())
}

/// A random UUID (version 4, lowercase, hyphenated).
pub(crate) fn uuid_v4() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| {
        StoreError::new(ErrorKind::Sqlite, format!("no random source for the server id: {e}"))
    })?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_ignore_line_ends() {
        let a = Migration::new(1, "001_a", "CREATE TABLE a (x INTEGER);\nSELECT 1;\n");
        let b = Migration::new(1, "001_a", "CREATE TABLE a (x INTEGER);\r\nSELECT 1;\r\n");
        assert_eq!(a.checksum(), b.checksum());
        assert_eq!(a.checksum().len(), 64);
    }

    #[test]
    fn uuids_are_version_4() {
        let u = uuid_v4().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(u, uuid_v4().unwrap());
    }

    #[test]
    fn embedded_migrations_are_numbered_from_one() {
        let list = embedded();
        for (i, m) in list.iter().enumerate() {
            assert_eq!(m.version, i as i64 + 1);
            assert!(m.name.starts_with(&format!("{:03}_", m.version)));
        }
    }
}
