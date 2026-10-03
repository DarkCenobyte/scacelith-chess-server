//! `backup <file> [--verify]`: a consistent copy of the database.
//!
//! `VACUUM INTO` writes a consistent snapshot in one pass (the sqlite3 shell's `.backup` copies
//! 100 pages at a time and starts over whenever another connection writes, so on a busy server it
//! may never finish). The copy holds e-mail addresses and recent IPs: it is created with mode 600,
//! and should be encrypted before it leaves the host. The source must be the server's database: a
//! missing file (opening it would create an empty one) or a database without the schema (a wrong
//! `DB_PATH` or `DATA_DIR`) is refused, so that a scheduled backup never silently copies nothing.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::json;

use super::{Ctx, Failure, Output, refuse};
use crate::util::js;

/// What a backup wrote.
struct Written {
    file: PathBuf,
    bytes: u64,
    ms: i64,
    verified: bool,
}

pub(super) async fn backup(ctx: &Ctx) -> Result<Output, Failure> {
    let Some(target) = ctx.positional(1).filter(|t| !t.is_empty()).map(str::to_string) else {
        return Err(refuse("backup <file>: the new file to write"));
    };
    let src = ctx.config.db_path.clone();
    let memory = Path::new(&src).file_name().is_some_and(|n| n == ":memory:");
    if src.is_empty() || src == ":memory:" || memory {
        return Err(refuse("no database file to back up (DB_PATH)"));
    }
    let verify = ctx.on("verify");
    let w = tokio::task::spawn_blocking(move || copy(&src, &target, verify))
        .await
        .map_err(|e| Failure::Failed(e.to_string()))??;
    let file = w.file.display().to_string();
    let text = format!(
        "Backup written to {file} ({} MB in {} ms{}).\n",
        js::to_fixed(w.bytes as f64 / 1_048_576.0, 1),
        w.ms,
        if w.verified { ", quick_check ok" } else { "" }
    );
    let data = json!({ "file": file, "bytes": w.bytes, "ms": w.ms, "verified": w.verified });
    Ok(Output::new(data, text))
}

/// Copies the database `src` into the new file `target` (blocking: on the blocking pool).
fn copy(src: &str, target: &str, verify: bool) -> Result<Written, Failure> {
    let not_ours =
        |why: &str| refuse(format!("no Scacelith database at {src} ({why}); check DB_PATH and DATA_DIR"));
    if !Path::new(src).exists() {
        return Err(not_ours("no such file"));
    }
    let out = std::path::absolute(target).map_err(|e| refuse(format!("cannot create {target}: {e}")))?;
    let started = Instant::now();
    // Never created: the file exists (checked above), a read-write open without CREATE.
    let db =
        Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(|e| not_ours(&e.to_string()))?;
    let schema = db.busy_timeout(std::time::Duration::from_millis(5000)).and_then(|()| {
        let table: Option<i64> = db
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if table.is_none() {
            return Ok(false);
        }
        let applied: Option<i64> =
            db.query_row("SELECT 1 FROM schema_migrations LIMIT 1", [], |r| r.get(0)).optional()?;
        Ok(applied.is_some())
    });
    match schema {
        Ok(true) => {}
        Ok(false) => return Err(not_ours("no schema_migrations")),
        Err(e) => return Err(not_ours(&e.to_string())),
    }
    // VACUUM INTO accepts an empty file: created first, with its mode, never over another one.
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(&out).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            refuse(format!("{target} exists: choose a new file"))
        } else {
            refuse(format!("cannot create {target}: {e}"))
        }
    })?;
    if let Err(e) = db.execute("VACUUM INTO ?1", [out.display().to_string()]) {
        let _ = std::fs::remove_file(&out);
        return Err(refuse(format!("backup failed: {e}")));
    }
    drop(db);
    let ms = started.elapsed().as_millis() as i64;
    let mut verified = false;
    if verify {
        let check = quick_check(&out).unwrap_or_else(|e| e.to_string());
        if check != "ok" {
            return Err(refuse(format!("backup written to {target} but quick_check failed: {check}")));
        }
        verified = true;
    }
    let bytes = std::fs::metadata(&out)?.len();
    Ok(Written { file: out, bytes, ms, verified })
}

/// `PRAGMA quick_check` of a file, its rows joined by `; `.
fn quick_check(path: &Path) -> rusqlite::Result<String> {
    let b = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = b.prepare("PRAGMA quick_check")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.join("; "))
}
