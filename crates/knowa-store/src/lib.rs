//! One replica directory of a space (DESIGN §5): manifest, write lock, per-actor
//! append-only logs, content-addressed blobs, verification and bundles.
//! The store knows nothing about status, visibility or queries.

mod blob;
mod bundle;
mod fsutil;
mod log;
mod verify;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use knowa_model::vocab::{FORMAT_VERSION, OWNER};
use knowa_model::{Claim, Error, ErrorCode, Id, IdKind, Result, Value};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "knowa.json";
pub const LOG_DIR: &str = "log";
pub const BLOB_DIR: &str = "blobs";
pub const INDEX_DIR: &str = ".index";
pub const LOCK_FILE: &str = ".lock";
pub const SEGMENT_LINES: u64 = 10_000;
/// `<root>/.index/needs-rebuild`: written (and fsynced) by `merge` before any segment,
/// because index catch-up cannot see segments that fill gaps below an indexed head
/// (DESIGN §5); `import_bundle` also writes it before any blob it adds, because catch-up
/// cannot see a newly arrived blob either. `Index::open` rebuilds while it exists and
/// removes it afterwards.
pub const REBUILD_MARKER: &str = "needs-rebuild";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub space: Id,
    pub owner: Id,
    pub genesis: Id,
}

/// Exclusive advisory lock on `<root>/.lock` (std `File::lock`). Released on drop.
pub struct WriteLock {
    _file: File,
    /// the replica root this lock was taken on; mutations check it
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Head {
    pub seq: u64,
    pub id: Id,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VerifyReport {
    pub claims: u64,
    pub blobs: u64,
    pub actors: u64,
    /// per actor: (first missing seq, last missing seq) runs — expected only in partial replicas
    pub gaps: Vec<(Id, u64, u64)>,
    /// human-readable problems; non-empty ⇒ `verify()` returns `integrity` error with these in detail
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImportReport {
    pub added: u64,
    pub skipped_existing: u64,
    pub blobs_added: u64,
    pub gaps: Vec<(Id, u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleMeta {
    pub format: u32,
    pub space: Id,
    pub owner: Id,
    pub genesis: Id,
    /// "private" | "shared" | "public"
    pub level: String,
    pub partial: bool,
    pub exported_by: Id,
    /// canonical timestamp
    pub at: String,
}

pub struct Replica {
    root: PathBuf,
    manifest: Manifest,
}

impl Replica {
    /// Creates a new replica at `dir` (must not exist, or be an empty directory),
    /// writes the manifest and appends `genesis` (seq 1, `knowa:owner`, signed by owner).
    /// `genesis` must verify; `manifest.genesis == genesis.id`.
    pub fn create(dir: &Path, genesis: &Claim) -> Result<Replica> {
        genesis.verify()?;
        if let Some(p) = genesis_problem(genesis) {
            return Err(Error::invalid(p));
        }
        let b = &genesis.body;
        let manifest = Manifest {
            format: FORMAT_VERSION,
            space: b.space.clone(),
            owner: b.actor.clone(),
            genesis: genesis.id.clone(),
        };
        let replica = Replica::prepare(dir, manifest)?;
        let lock = replica.lock()?;
        replica.append(&lock, std::slice::from_ref(genesis))?;
        // The manifest goes last: a directory with `knowa.json` is a complete replica.
        replica.write_manifest()?;
        Ok(replica)
    }

    /// Opens an existing replica; missing/invalid manifest → `not_found`/`integrity`.
    pub fn open(dir: &Path) -> Result<Replica> {
        let path = dir.join(MANIFEST_FILE);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::not_found(format!("{} is not a Knowa replica (no {MANIFEST_FILE})", dir.display())));
            }
            Err(e) => return Err(fsutil::io_err(&path, e)),
        };
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| Error::integrity(format!("{}: {e}", path.display())))?;
        if manifest.format != FORMAT_VERSION {
            return Err(Error::unsupported(format!(
                "{}: format {} (this build reads format {FORMAT_VERSION})",
                path.display(),
                manifest.format
            )));
        }
        if let Some(p) = manifest_kind_problem(&manifest) {
            return Err(Error::integrity(format!("{}: {p}", path.display())));
        }
        Ok(Replica { root: dir.to_path_buf(), manifest })
    }

    /// Creates an empty replica for an *existing* space (import target). `dir` as in `create`.
    pub fn create_replica(dir: &Path, manifest: &Manifest) -> Result<Replica> {
        if manifest.format != FORMAT_VERSION {
            return Err(Error::invalid(format!("manifest format {} (expected {FORMAT_VERSION})", manifest.format)));
        }
        if let Some(p) = manifest_kind_problem(manifest) {
            return Err(Error::invalid(format!("manifest: {p}")));
        }
        let replica = Replica::prepare(dir, manifest.clone())?;
        replica.write_manifest()?;
        Ok(replica)
    }

    /// Creates `dir` (or accepts it empty) with `log/` and `blobs/`; no manifest yet.
    fn prepare(dir: &Path, manifest: Manifest) -> Result<Replica> {
        match fs::read_dir(dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(Error::conflict(format!("{} exists and is not empty", dir.display())));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if dir.exists() && !dir.is_dir() => {
                return Err(Error::conflict(format!("{} exists and is not a directory ({e})", dir.display())));
            }
            Err(e) => return Err(fsutil::io_err(dir, e)),
        }
        fsutil::ensure_dir(&dir.join(LOG_DIR))?;
        fsutil::ensure_dir(&dir.join(BLOB_DIR))?;
        Ok(Replica { root: dir.to_path_buf(), manifest })
    }

    /// A lock taken on another replica protects nothing here: that is a caller bug.
    fn check_lock(&self, lock: &WriteLock) -> Result<()> {
        if lock.root != self.root {
            return Err(Error::internal(format!(
                "write lock for {} used on replica {}",
                lock.root.display(),
                self.root.display()
            )));
        }
        Ok(())
    }

    fn write_manifest(&self) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.root.join(MANIFEST_FILE), &bytes)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Blocks until the exclusive lock is held, then repairs torn tails: an unterminated
    /// final line in an actor's newest segment is an unacknowledged crashed append
    /// (DESIGN §5) and is truncated.
    pub fn lock(&self) -> Result<WriteLock> {
        let path = self.root.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| fsutil::io_err(&path, e))?;
        file.lock().map_err(|e| fsutil::io_err(&path, e))?;
        let lock = WriteLock { _file: file, root: self.root.clone() };
        // Only now: the repair is safe only while no append can be in flight.
        log::repair_torn_tails(&self.root)?;
        Ok(lock)
    }

    /// Last claim of `actor` in this replica (highest seq), if any.
    pub fn head(&self, lock: &WriteLock, actor: &Id) -> Result<Option<Head>> {
        self.check_lock(lock)?;
        Ok(log::tail(&self.root, &self.manifest.space, actor)?.map(|t| t.head))
    }

    /// Appends locally-authored claims. For each actor, claims must continue that
    /// actor's chain exactly (seq = head+1, prev = head id), in order; each must verify.
    /// Writes JCS lines, rotates segments at SEGMENT_LINES, fsyncs before returning.
    /// Violations → `conflict` (chain) / `integrity` (verify); nothing is written on error,
    /// and a write that fails part-way is truncated back to the pre-append state.
    pub fn append(&self, lock: &WriteLock, claims: &[Claim]) -> Result<()> {
        self.check_lock(lock)?;
        log::append(&self.root, &self.manifest.space, claims)
    }

    /// Merges foreign claims (bundle import). Each must verify and belong to this space.
    /// Already-present (actor, seq) with the same id → skipped; same (actor, seq) with a
    /// different id → `integrity` (equivocation). Gaps are allowed and reported.
    /// Where seq n-1 is present, `prev` must equal its id → else `integrity`.
    /// If anything is new, `<root>/.index/<REBUILD_MARKER>` is written and fsynced
    /// before the first segment (DESIGN §5).
    pub fn merge(&self, lock: &WriteLock, claims: &[Claim]) -> Result<ImportReport> {
        self.check_lock(lock)?;
        let space = &self.manifest.space;
        for c in claims {
            log::check_claim(c, space, ErrorCode::Integrity)?;
        }
        let plan = log::plan_merge(claims, |actor| log::chain(&self.root, space, actor))?;
        if plan.writes_segments() {
            mark_rebuild(&self.root)?;
        }
        plan.apply(&self.root)
    }

    /// Actors that have a log directory.
    pub fn actors(&self) -> Result<Vec<Id>> {
        let (actors, problems) = log::scan_actors(&self.root)?;
        log::fail_on(problems)?;
        Ok(actors)
    }

    /// Claims of `actor` with seq ≥ `from_seq`, ascending by seq. Every claim is
    /// verified (id + signature, `Claim::verify`) before it is yielded: logs are never
    /// trusted (DESIGN §5). Failure → `integrity`.
    pub fn read_log(&self, actor: &Id, from_seq: u64) -> Result<Box<dyn Iterator<Item = Result<Claim>> + '_>> {
        Ok(Box::new(log::LogIter::new(&self.root, &self.manifest.space, actor, from_seq)?))
    }

    /// Streams `r` into the blob store: temp file in `blobs/`, sha256 while copying,
    /// rename into place. Returns (id, created, size).
    pub fn put_blob(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
        blob::put(&self.root, r)
    }

    /// Accepts a `b_` id with or without fragment; fragment ignored.
    pub fn has_blob(&self, id: &Id) -> bool {
        // Non-blob ids are simply not present; the path cannot fail for a blob id.
        blob::path(&self.root, id).is_ok_and(|p| p.is_file())
    }

    /// Absent → `not_found`.
    pub fn blob_path(&self, id: &Id) -> Result<PathBuf> {
        let path = blob::path(&self.root, id)?;
        if !path.is_file() {
            return Err(Error::not_found(format!("blob {} not found in {}", id.base(), self.root.display())));
        }
        Ok(path)
    }

    pub fn blob_ids(&self) -> Result<Vec<Id>> {
        let (blobs, problems) = blob::scan(&self.root)?;
        log::fail_on(problems)?;
        Ok(blobs.into_iter().map(|(id, _)| id).collect())
    }

    /// `<root>/.index`, created if missing.
    pub fn index_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join(INDEX_DIR);
        fsutil::ensure_dir(&dir)?;
        Ok(dir)
    }

    /// Checks every claim (verify, belongs to space, chain within contiguous runs,
    /// no duplicate seq), every blob hash, manifest genesis presence.
    pub fn verify(&self) -> Result<VerifyReport> {
        verify::verify(self)
    }
}

/// A genesis is seq 1 `<space> knowa:owner {"ref":"<author>"}` by its author (DESIGN §4.6).
pub(crate) fn genesis_problem(genesis: &Claim) -> Option<String> {
    let b = &genesis.body;
    let owner_ref = Value::Ref(b.actor.clone());
    (b.seq != 1 || b.p.as_str() != OWNER || b.s != b.space || b.o != owner_ref).then(|| {
        format!("genesis {} must be seq 1 `{} {OWNER} {{\"ref\":\"{}\"}}` by its owner", genesis.id, b.space, b.actor)
    })
}

/// `<root>/.index/<REBUILD_MARKER>`.
pub(crate) fn rebuild_marker(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(REBUILD_MARKER)
}

/// Creates and fsyncs the rebuild marker (and `.index/` if needed) so it is durable
/// before anything it covers is written.
pub(crate) fn mark_rebuild(root: &Path) -> Result<()> {
    fsutil::ensure_dir(&root.join(INDEX_DIR))?;
    fsutil::atomic_write(&rebuild_marker(root), b"")
}

fn manifest_kind_problem(m: &Manifest) -> Option<String> {
    [("space", &m.space, IdKind::Space), ("owner", &m.owner, IdKind::Actor), ("genesis", &m.genesis, IdKind::Claim)]
        .into_iter()
        .find(|(_, id, kind)| id.kind() != *kind)
        .map(|(field, id, kind)| format!("{field} {id} is not a {kind:?} id"))
}

/// Writes a tar bundle: `bundle.json`, then `log/<actor>/<segment>.jsonl` containing
/// exactly `claims` (grouped per actor, ascending seq), then `blobs/<..>/<id>` for `blobs`.
/// Selecting which claims/blobs is the caller's job.
pub fn write_bundle(replica: &Replica, out: &Path, meta: &BundleMeta, claims: &[Claim], blobs: &[Id]) -> Result<()> {
    bundle::write(replica, out, meta, claims, blobs)
}

/// Reads only `bundle.json` (no verification of the rest) — e.g. to choose an import
/// destination by space id before importing.
pub fn read_bundle_meta(bundle: &Path) -> Result<BundleMeta> {
    bundle::read_meta(bundle)
}

/// Imports a bundle into `dest` (created via `create_replica` if absent; if present its
/// manifest must match the bundle's space). Verifies all claim signatures and blob hashes,
/// and that the bundle's genesis is a genesis claim, before writing anything. Writes the
/// rebuild marker before the first blob or segment it adds (as `Replica::merge`).
pub fn import_bundle(bundle: &Path, dest: &Path) -> Result<(Replica, BundleMeta, ImportReport)> {
    bundle::import(bundle, dest)
}
