//! Who a shard's journal directory belongs to. `JOURNAL_DIR/shard-<n>/owner` holds the server id
//! of the database whose games the journal holds (written at the first start of a version that
//! knows it; a directory of 0.9.1 or older has none), and the process that serves the shard holds
//! an exclusive lock on that file (`flock`) for as long as it runs, released by the kernel when the
//! process ends. The journal itself never reads the file: segments are the `segment-<seq>.log`
//! files only.
//!
//! The game hosts use it at start (`game::host::Hosts::start`): a shard directory locked by
//! another process is never opened, and the journal of another database is never replayed into
//! this one.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::format::segment_seq;
use crate::ids::MAX_SHARDS;

/// Name of the owner file in a shard's directory.
pub const OWNER_FILE: &str = "owner";

/// The longest owner file read (a server id is 32 hex digits).
const OWNER_MAX: u64 = 256;

/// The shard numbers of the `shard-<n>` directories under `root` (`n` below 64, written without
/// leading zeros), in increasing order. A missing `root` has none.
pub fn shard_dirs(root: &Path) -> io::Result<Vec<u32>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut shards = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(shard) = entry.file_name().to_str().and_then(shard_of_dir) else { continue };
        if entry.file_type()?.is_dir() {
            shards.push(shard);
        }
    }
    shards.sort_unstable();
    Ok(shards)
}

/// Whether a shard's directory holds journal segments (a directory without any has no game to
/// recover).
pub fn has_segments(root: &Path, shard: u32) -> io::Result<bool> {
    let entries = match fs::read_dir(root.join(format!("shard-{shard}"))) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for entry in entries {
        if entry?.file_name().to_str().and_then(segment_seq).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The shard of a directory name `shard-<n>`.
fn shard_of_dir(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("shard-")?;
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok().filter(|&n| n < MAX_SHARDS)
}

/// Why a shard directory could not be claimed.
#[derive(Debug)]
pub enum ClaimError {
    /// Another process holds the lock: it serves this shard.
    InUse,
    /// The directory or its owner file could not be created, opened or read.
    Io(io::Error),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::InUse => f.write_str("in use by another process"),
            ClaimError::Io(e) => write!(f, "owner file: {e}"),
        }
    }
}

impl std::error::Error for ClaimError {}

/// A shard directory claimed by this process: its owner file, locked until the claim is dropped.
#[derive(Debug)]
pub struct ShardClaim {
    shard: u32,
    path: PathBuf,
    file: File,
    owner: Option<String>,
    locked: bool,
}

impl ShardClaim {
    /// Claims `root/shard-<shard>` (created if needed): opens its owner file (created empty if
    /// needed), locks it and reads the server id it holds. A file system without `flock` gives an
    /// unlocked claim ([`ShardClaim::locked`]).
    ///
    /// # Errors
    ///
    /// [`ClaimError::InUse`] when another process holds the lock, [`ClaimError::Io`] otherwise.
    pub fn take(root: &Path, shard: u32) -> Result<ShardClaim, ClaimError> {
        let dir = root.join(format!("shard-{shard}"));
        fs::create_dir_all(&dir).map_err(ClaimError::Io)?;
        let path = dir.join(OWNER_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(ClaimError::Io)?;
        let locked = match file.try_lock() {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => return Err(ClaimError::InUse),
            Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => false,
            Err(TryLockError::Error(e)) => return Err(ClaimError::Io(e)),
        };
        let mut text = String::new();
        (&file).take(OWNER_MAX).read_to_string(&mut text).map_err(ClaimError::Io)?;
        let owner = Some(text.trim().to_owned()).filter(|s| !s.is_empty());
        Ok(ShardClaim { shard, path, file, owner, locked })
    }

    /// The shard.
    #[must_use]
    pub fn shard(&self) -> u32 {
        self.shard
    }

    /// The server id the owner file holds (`None`: none yet).
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Whether the lock is held (false only on a file system without `flock`).
    #[must_use]
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Records `server_id` as the owner (written and synced when it changes).
    ///
    /// # Errors
    ///
    /// The write or the sync failed.
    pub fn set_owner(&mut self, server_id: &str) -> io::Result<()> {
        if self.owner.as_deref() == Some(server_id) {
            return Ok(());
        }
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(format!("{server_id}\n").as_bytes())?;
        self.file.sync_all()?;
        self.owner = Some(server_id.to_owned());
        Ok(())
    }

    /// The owner file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
