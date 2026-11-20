//! Filesystem primitives shared by the store: in-flight temp files, atomic
//! publication (temp + fsync + rename + dir fsync), and error mapping.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use knowa_model::{Error, Result};

/// Prefix of every in-flight file the store creates. Directory scans skip
/// dot-prefixed entries: such a file is a write whose final rename never happened,
/// so it holds nothing committed.
pub(crate) const TEMP_PREFIX: &str = ".tmp-";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn io_err(path: &Path, e: io::Error) -> Error {
    Error::internal(format!("io: {}: {e}", path.display()))
}

/// True for directory entries that scans must skip (see `TEMP_PREFIX`; also OS
/// metadata such as `.DS_Store`, which never holds Knowa data).
pub(crate) fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// The UTF-8 name of a directory entry; non-UTF-8 names cannot be Knowa files.
pub(crate) fn entry_name(entry: &fs::DirEntry) -> std::result::Result<String, String> {
    entry.file_name().into_string().map_err(|_| format!("{}: non-UTF-8 file name", entry.path().display()))
}

/// Lists `dir`; `Ok(None)` when it does not exist.
pub(crate) fn read_dir_if_exists(dir: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(Some(rd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(dir, e)),
    }
}

pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io_err(dir, e))
}

/// `create_dir_all`, then fsyncs the parent of every directory it created so the
/// new entries survive a crash.
pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_dir(parent)?;
    }
    match fs::create_dir(dir) {
        Ok(()) => {}
        // Another writer created it between our check and create; the directory
        // exists either way, which is all the caller needs.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => return Ok(()),
        Err(e) => return Err(io_err(dir, e)),
    }
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => sync_dir(parent),
        _ => Ok(()),
    }
}

/// A file being written under a temp name. Publish it with `commit`; dropping it
/// uncommitted deletes it.
pub(crate) struct TempFile {
    path: PathBuf,
    file: File,
    armed: bool,
}

impl TempFile {
    pub(crate) fn create_in(dir: &Path) -> Result<TempFile> {
        loop {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{TEMP_PREFIX}{}-{n}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok(TempFile { path, file, armed: true }),
                // A crashed earlier process with the same pid left this name behind.
                // Its content is uncommitted; take the next name instead of touching it.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_err(&path, e)),
            }
        }
    }

    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// fsyncs the content, renames it to `dest` (replacing any existing file), and
    /// fsyncs `dest`'s directory.
    pub(crate) fn commit(mut self, dest: &Path) -> Result<()> {
        self.file.sync_all().map_err(|e| io_err(&self.path, e))?;
        fs::rename(&self.path, dest).map_err(|e| io_err(dest, e))?;
        self.armed = false;
        match dest.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => sync_dir(parent),
            _ => sync_dir(Path::new(".")),
        }
    }

    /// Deletes the temp file, reporting a failure to do so.
    pub(crate) fn discard(mut self) -> Result<()> {
        self.armed = false;
        fs::remove_file(&self.path).map_err(|e| io_err(&self.path, e))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            // Error path only: the caller is already returning the error that got us
            // here. A temp file we fail to delete is dot-prefixed and skipped by every
            // scan, so leaving it behind is harmless.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Atomically replaces `dest` with `bytes` (temp in the same directory + rename).
pub(crate) fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = TempFile::create_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    tmp.file().write_all(bytes).map_err(|e| io_err(&tmp_path, e))?;
    tmp.commit(dest)
}
