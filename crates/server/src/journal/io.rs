//! The file-system operations of the journal, each announced to the optional [`IoProbe`] first
//! (tests record the order of the syncs and deletions, and inject failures).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

/// A file-system operation of the journal, as an [`IoProbe`] sees it (before it happens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOp {
    /// Creates segment `seq` (rotation).
    Create(u64),
    /// Writes a batch of `len` bytes to segment `seq`.
    Write { seq: u64, len: usize },
    /// fdatasync of segment `seq`.
    Datasync(u64),
    /// fsync of the shard's directory.
    SyncDir,
    /// Deletes segment `seq`.
    Unlink(u64),
}

/// Observes each file-system operation of a journal before it happens; an error replaces the
/// operation's outcome (fault injection). Runs on the journal's I/O thread.
pub type IoProbe = Arc<dyn Fn(IoOp) -> io::Result<()> + Send + Sync>;

/// The file-system operations, with the probe.
pub(super) struct Io {
    probe: Option<IoProbe>,
}

impl Io {
    pub fn new(probe: Option<IoProbe>) -> Io {
        Io { probe }
    }

    fn announce(&self, op: IoOp) -> io::Result<()> {
        match &self.probe {
            Some(probe) => probe(op),
            None => Ok(()),
        }
    }

    /// Opens a new segment for appending.
    pub fn create(&self, path: &Path, seq: u64) -> io::Result<File> {
        self.announce(IoOp::Create(seq))?;
        OpenOptions::new().append(true).create(true).open(path)
    }

    /// Writes all of `buf`; `written` counts the bytes that reached the file, even on error.
    pub fn write(&self, file: &mut File, seq: u64, buf: &[u8], written: &mut u64) -> io::Result<()> {
        self.announce(IoOp::Write { seq, len: buf.len() })?;
        let mut rest = buf;
        while !rest.is_empty() {
            match file.write(rest) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "journal segment write returned 0"));
                }
                Ok(n) => {
                    *written += n as u64;
                    rest = &rest[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// fdatasync of an open segment.
    pub fn datasync(&self, file: &File, seq: u64) -> io::Result<()> {
        self.announce(IoOp::Datasync(seq))?;
        file.sync_data()
    }

    /// Reads a whole segment; with `sync`, makes it durable first (best effort: it may hold
    /// records a process wrote just before dying, which only the page cache has).
    pub fn read_segment(&self, path: &Path, seq: u64, sync: bool) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        if sync {
            let _ = self.announce(IoOp::Datasync(seq)).and_then(|()| file.sync_data());
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// fdatasync of a segment by path (best effort).
    pub fn datasync_path(&self, path: &Path, seq: u64) {
        let _ = self.announce(IoOp::Datasync(seq)).and_then(|()| File::open(path)?.sync_data());
    }

    /// fsync of a directory (best effort): the entries created and deleted so far are durable.
    pub fn sync_dir(&self, dir: &Path) {
        let _ = self.announce(IoOp::SyncDir).and_then(|()| File::open(dir)?.sync_all());
    }

    /// Deletes a segment.
    pub fn unlink(&self, path: &Path, seq: u64) -> io::Result<()> {
        self.announce(IoOp::Unlink(seq))?;
        fs::remove_file(path)
    }
}
