//! Content-addressed blobs: `blobs/<id[6..8]>/<id>`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use knowa_model::{Error, Id, IdKind, Result};
use sha2::{Digest, Sha256};

use crate::BLOB_DIR;
use crate::fsutil::{self, TempFile, io_err};

const COPY_BUF: usize = 64 * 1024;

/// Blob files found by a scan, plus entries that do not belong under `blobs/`.
pub(crate) type Scan = (Vec<(Id, PathBuf)>, Vec<String>);

/// Streaming SHA-256 → blob id.
struct BlobHasher {
    hasher: Sha256,
}

impl BlobHasher {
    fn new() -> BlobHasher {
        BlobHasher { hasher: Sha256::new() }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    fn finish(self) -> Id {
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

fn hash_file(path: &Path) -> Result<Id> {
    let mut f = File::open(path).map_err(|e| io_err(path, e))?;
    Ok(hash_reader(&mut f, &|| path.display().to_string())?.0)
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

pub(crate) fn path(root: &Path, id: &Id) -> Result<PathBuf> {
    let id = require_blob(id)?;
    Ok(root.join(BLOB_DIR).join(fanout(&id)?).join(id.as_str()))
}

/// Streams `r` into a temp file inside `blobs/` while hashing, then renames it into
/// place. An already-present blob is left untouched and the temp file removed.
pub(crate) fn put(root: &Path, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
    let dir = root.join(BLOB_DIR);
    fsutil::ensure_dir(&dir)?;
    let mut tmp = TempFile::create_in(&dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let mut hasher = BlobHasher::new();
    let size = pump(
        r,
        |chunk| {
            tmp.file().write_all(chunk).map_err(|e| io_err(&tmp_path, e))?;
            hasher.update(chunk);
            Ok(())
        },
        &|| "blob input".to_string(),
    )?;
    let id = hasher.finish();
    let dest = path(root, &id)?;
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

/// Every blob file under `blobs/`.
pub(crate) fn scan(root: &Path) -> Result<Scan> {
    let dir = root.join(BLOB_DIR);
    let (mut blobs, mut problems) = (Vec::new(), Vec::new());
    // `blobs/` absent: tools such as git drop empty directories; no directory means
    // no blobs, exactly as an empty one does.
    let Some(entries) = fsutil::read_dir_if_exists(&dir)? else {
        return Ok((blobs, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
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

/// Recomputes the hash of the blob at `path` and compares it with `id`.
pub(crate) fn check(id: &Id, path: &Path) -> Result<Option<String>> {
    let got = hash_file(path)?;
    Ok((got != *id).then(|| format!("{}: content hashes to {got}", path.display())))
}
