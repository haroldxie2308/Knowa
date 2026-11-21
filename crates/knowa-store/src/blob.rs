//! Content-addressed blobs: `blobs/<id[6..8]>/<id>`. Hashing and naming are shared by
//! every store. Storing, listing and paths work on a blob directory, the one holding
//! the fan-out directories: a directory replica's `blobs/` (`dir::DirBlobs`), or this
//! machine's blob cache, through which an object store's blobs pass (`object`).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use knowa_model::{Error, Id, IdKind, Result};
use sha2::{Digest, Sha256};

use crate::BLOB_DIR;
use crate::fsutil::{self, TempFile, TempPath, io_err};

const COPY_BUF: usize = 64 * 1024;

/// Blob files found by a scan, plus entries that do not belong under `blobs/`.
pub(crate) type Scan = (Vec<(Id, PathBuf)>, Vec<String>);

/// Streaming SHA-256 → blob id.
pub(crate) struct BlobHasher {
    hasher: Sha256,
}

impl BlobHasher {
    pub(crate) fn new() -> BlobHasher {
        BlobHasher { hasher: Sha256::new() }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    pub(crate) fn finish(self) -> Id {
        Id::blob_from_sha256(&self.hasher.finalize().into())
    }
}

/// Reads `r` to EOF in chunks, handing each chunk to `sink`. Returns the byte count.
fn pump(r: &mut dyn Read, mut sink: impl FnMut(&[u8]) -> Result<()>, what: &dyn Fn() -> String) -> Result<u64> {
    let mut buf = vec![0u8; COPY_BUF];
    let mut size = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => return Ok(size),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::internal(format!("io: {}: {e}", what()))),
        };
        sink(&buf[..n])?;
        size += n as u64;
    }
}

/// Hashes everything `r` yields.
pub(crate) fn hash_reader(r: &mut dyn Read, what: &dyn Fn() -> String) -> Result<(Id, u64)> {
    let mut hasher = BlobHasher::new();
    let size = pump(
        r,
        |chunk| {
            hasher.update(chunk);
            Ok(())
        },
        what,
    )?;
    Ok((hasher.finish(), size))
}

/// Hashes the file at `src` in place, storing nothing (DESIGN §5 Catalog-only blobs).
pub(crate) fn hash_file(src: &Path) -> Result<(Id, u64)> {
    regular_source(src)?;
    let mut f = File::open(src).map_err(|e| source_err(src, e))?;
    hash_reader(&mut f, &|| src.display().to_string())
}

fn source_err(src: &Path, e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("blob source not found: {}", src.display())),
        _ => io_err(src, e),
    }
}

/// The metadata of `src`, which must be a regular file. Checked before `src` is
/// opened or copied: opening a FIFO would block.
fn regular_source(src: &Path) -> Result<fs::Metadata> {
    let meta = fs::metadata(src).map_err(|e| source_err(src, e))?;
    if !meta.is_file() {
        return Err(Error::invalid(format!("blob source is not a regular file: {}", src.display())));
    }
    Ok(meta)
}

/// Two fan-out characters taken from the digest. `id[2..5]` is always `ciq` (the
/// multihash header 0x12 0x20) and `id[5]` mixes header and digest bits, so the first
/// fully digest-derived characters are `id[6..8]` (DESIGN §5).
fn fanout(id: &Id) -> Result<&str> {
    id.as_str().get(6..8).ok_or_else(|| Error::internal(format!("malformed blob id {id}")))
}

fn require_blob(id: &Id) -> Result<Id> {
    if id.kind() != IdKind::Blob {
        return Err(Error::invalid(format!("{id} is not a blob id")));
    }
    Ok(id.base())
}

/// `blobs/<fanout>/<id>` relative to the replica root (also the bundle entry path).
pub(crate) fn rel_path(id: &Id) -> Result<String> {
    let id = require_blob(id)?;
    Ok(format!("{BLOB_DIR}/{}/{id}", fanout(&id)?))
}

/// Inverse of `rel_path`: only exact, fragment-free blob paths parse.
pub(crate) fn parse_rel_path(rel: &str) -> Option<Id> {
    let mut parts = rel.split('/');
    let (Some(BLOB_DIR), Some(dir), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let id = Id::parse(name).ok().filter(|id| id.kind() == IdKind::Blob && id.fragment().is_none())?;
    (fanout(&id).ok()? == dir).then_some(id)
}

/// `<dir>/<fanout>/<id>`, where `dir` is a blob directory.
pub(crate) fn path(dir: &Path, id: &Id) -> Result<PathBuf> {
    let id = require_blob(id)?;
    Ok(dir.join(fanout(&id)?).join(id.as_str()))
}

/// Streams `r` into a temp file inside the blob directory `dir` while hashing. Returns
/// the staged file, its id and its size.
fn stage(dir: &Path, r: &mut dyn Read, what: &dyn Fn() -> String) -> Result<(TempFile, Id, u64)> {
    fsutil::ensure_dir(dir)?;
    let mut tmp = TempFile::create_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let mut hasher = BlobHasher::new();
    let size = pump(
        r,
        |chunk| {
            tmp.file().write_all(chunk).map_err(|e| io_err(&tmp_path, e))?;
            hasher.update(chunk);
            Ok(())
        },
        what,
    )?;
    Ok((tmp, hasher.finish(), size))
}

/// Streams `r` into the blob directory `dir` while hashing, then publishes it.
pub(crate) fn put(dir: &Path, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
    let (tmp, id, size) = stage(dir, r, &|| "blob input".to_string())?;
    publish(dir, tmp, id, size)
}

/// Stores `r`'s bytes in the blob directory `dir` only if they hash to `expected`;
/// otherwise nothing is stored and the result is `integrity` (DESIGN §5.3: downloaded
/// bytes must equal their id before they are used). Returns the stored file.
pub(crate) fn put_expected(dir: &Path, r: &mut dyn Read, expected: &Id, what: &dyn Fn() -> String) -> Result<PathBuf> {
    let expected = require_blob(expected)?;
    let (tmp, got, size) = stage(dir, r, what)?;
    if got != expected {
        // Dropped unpublished, the temp file is removed (see `TempPath`'s drop): the
        // mismatch is the error to report.
        drop(tmp);
        return Err(Error::integrity(format!("{}: content hashes to {got}, not to its id {expected}", what())));
    }
    publish(dir, tmp, got, size)?;
    path(dir, &expected)
}

/// Stores the file at `src` (DESIGN §5 Blob path): `fs::copy` onto a fresh temp name
/// inside the blob directory `dir`, which clones where the filesystem supports it (std tries
/// `fclonefileat` first on Apple targets, and that works only onto a path that does
/// not exist yet), then hashes the temp file, never `src`, and publishes it. A locked
/// source is streamed instead (see `has_lock_flags`).
pub(crate) fn put_file(dir: &Path, src: &Path) -> Result<(Id, bool, u64)> {
    // Checked up front, not left to `fs::copy`.
    let meta = regular_source(src)?;
    if has_lock_flags(&meta) {
        // Fallback: a locked source (Finder "Locked", `chflags uchg`). macOS copies BSD
        // file flags onto a clone and onto an `fcopyfile` copy alike, and std cannot
        // clear them, so the temp file could be neither chmod-ed, renamed nor deleted.
        // Streaming makes a fresh file without flags: still a byte copy, hashed as it
        // is written, so the stored bytes match their id. Only the clone is lost.
        let mut f = File::open(src).map_err(|e| source_err(src, e))?;
        return put(dir, &mut f);
    }
    fsutil::ensure_dir(dir)?;
    let perms = fsutil::fresh_file_permissions(dir)?;
    let reserved = TempPath::reserve_in(dir)?;
    fs::copy(src, reserved.path()).map_err(|e| {
        Error::internal(format!("io: copy {} to {}: {e}", src.display(), reserved.path().display()))
    })?;
    let mut tmp = reserved.open()?;
    let tmp_path = tmp.path().to_path_buf();
    // The copy carries the source's mode; a stored blob's mode must not depend on
    // which entry point stored it.
    tmp.file().set_permissions(perms).map_err(|e| io_err(&tmp_path, e))?;
    let (id, size) = hash_reader(tmp.file(), &|| tmp_path.display().to_string())?;
    publish(dir, tmp, id, size)
}

/// True if `meta` carries a BSD file flag that forbids chmod, rename or unlink of the
/// file. Values from macOS `<sys/stat.h>` (std has the field but not the constants).
#[cfg(target_os = "macos")]
fn has_lock_flags(meta: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_IMMUTABLE: u32 = 0x0000_0002;
    const UF_APPEND: u32 = 0x0000_0004;
    const SF_IMMUTABLE: u32 = 0x0002_0000;
    const SF_APPEND: u32 = 0x0004_0000;
    const SF_NOUNLINK: u32 = 0x0010_0000;
    meta.st_flags() & (UF_IMMUTABLE | UF_APPEND | SF_IMMUTABLE | SF_APPEND | SF_NOUNLINK) != 0
}

/// Elsewhere std's `fs::copy` creates a fresh destination file and copies no flags.
#[cfg(not(target_os = "macos"))]
fn has_lock_flags(_meta: &fs::Metadata) -> bool {
    false
}

/// Shared tail of every put: renames the hashed temp file to its id (fsync file,
/// rename, fsync dir). An already-present blob is left untouched and the temp file
/// removed, so each byte sequence is stored once per blob directory.
fn publish(dir: &Path, tmp: TempFile, id: Id, size: u64) -> Result<(Id, bool, u64)> {
    let dest = path(dir, &id)?;
    if dest.is_file() {
        tmp.discard()?;
        return Ok((id, false, size));
    }
    if let Some(parent) = dest.parent() {
        fsutil::ensure_dir(parent)?;
    }
    tmp.commit(&dest)?;
    Ok((id, true, size))
}

/// Every blob file in the blob directory `dir`.
pub(crate) fn scan(dir: &Path) -> Result<Scan> {
    let (mut blobs, mut problems) = (Vec::new(), Vec::new());
    // `blobs/` absent: tools such as git drop empty directories; no directory means
    // no blobs, exactly as an empty one does.
    let Some(entries) = fsutil::read_dir_if_exists(dir)? else {
        return Ok((blobs, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = match fsutil::entry_name(&entry) {
            Ok(n) => n,
            Err(p) => {
                problems.push(p);
                continue;
            }
        };
        if fsutil::is_hidden(&name) {
            continue;
        }
        let sub = entry.path();
        if !sub.is_dir() {
            problems.push(format!("{}: not a blob fan-out directory", sub.display()));
            continue;
        }
        for inner in fs::read_dir(&sub).map_err(|e| io_err(&sub, e))? {
            let inner = inner.map_err(|e| io_err(&sub, e))?;
            let file = match fsutil::entry_name(&inner) {
                Ok(n) => n,
                Err(p) => {
                    problems.push(p);
                    continue;
                }
            };
            if fsutil::is_hidden(&file) {
                continue;
            }
            match parse_rel_path(&format!("{BLOB_DIR}/{name}/{file}")) {
                Some(id) if inner.path().is_file() => blobs.push((id, inner.path())),
                _ => problems.push(format!("{}: not a blob file", inner.path().display())),
            }
        }
    }
    blobs.sort();
    Ok((blobs, problems))
}
