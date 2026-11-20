//! One replica directory of a space (DESIGN §5): manifest, write lock, per-actor
//! append-only logs, content-addressed blobs, verification and bundles.
//! The store knows nothing about status, visibility or queries.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use knowa_model::{Claim, Id, Result};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "knowa.json";
pub const LOG_DIR: &str = "log";
pub const BLOB_DIR: &str = "blobs";
pub const INDEX_DIR: &str = ".index";
pub const LOCK_FILE: &str = ".lock";
pub const SEGMENT_LINES: u64 = 10_000;

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
        let _ = (dir, genesis);
        todo!()
    }

    /// Opens an existing replica; missing/invalid manifest → `not_found`/`integrity`.
    pub fn open(dir: &Path) -> Result<Replica> {
        let _ = dir;
        todo!()
    }

    /// Creates an empty replica for an *existing* space (import target). `dir` as in `create`.
    pub fn create_replica(dir: &Path, manifest: &Manifest) -> Result<Replica> {
        let _ = (dir, manifest);
        todo!()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Blocks until the exclusive lock is held.
    pub fn lock(&self) -> Result<WriteLock> {
        todo!()
    }

    /// Last claim of `actor` in this replica (highest seq), if any.
    pub fn head(&self, _lock: &WriteLock, actor: &Id) -> Result<Option<Head>> {
        let _ = actor;
        todo!()
    }

    /// Appends locally-authored claims. For each actor, claims must continue that
    /// actor's chain exactly (seq = head+1, prev = head id), in order; each must verify.
    /// Writes JCS lines, rotates segments at SEGMENT_LINES, fsyncs before returning.
    /// Violations → `conflict` (chain) / `integrity` (verify); nothing is written on error.
    pub fn append(&self, _lock: &WriteLock, claims: &[Claim]) -> Result<()> {
        let _ = claims;
        todo!()
    }

    /// Merges foreign claims (bundle import). Each must verify and belong to this space.
    /// Already-present (actor, seq) with the same id → skipped; same (actor, seq) with a
    /// different id → `integrity` (equivocation). Gaps are allowed and reported.
    /// Where seq n-1 is present, `prev` must equal its id → else `integrity`.
    pub fn merge(&self, _lock: &WriteLock, claims: &[Claim]) -> Result<ImportReport> {
        let _ = claims;
        todo!()
    }

    /// Actors that have a log directory.
    pub fn actors(&self) -> Result<Vec<Id>> {
        todo!()
    }

    /// Claims of `actor` with seq ≥ `from_seq`, ascending by seq.
    pub fn read_log(&self, actor: &Id, from_seq: u64) -> Result<Box<dyn Iterator<Item = Result<Claim>> + '_>> {
        let _ = (actor, from_seq);
        todo!()
    }

    /// Streams `r` into the blob store: temp file in `blobs/`, sha256 while copying,
    /// rename into place. Returns (id, created, size).
    pub fn put_blob(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
        let _ = r;
        todo!()
    }

    /// Accepts a `b_` id with or without fragment; fragment ignored.
    pub fn has_blob(&self, id: &Id) -> bool {
        let _ = id;
        todo!()
    }

    /// Absent → `not_found`.
    pub fn blob_path(&self, id: &Id) -> Result<PathBuf> {
        let _ = id;
        todo!()
    }

    pub fn blob_ids(&self) -> Result<Vec<Id>> {
        todo!()
    }

    /// `<root>/.index`, created if missing.
    pub fn index_dir(&self) -> Result<PathBuf> {
        todo!()
    }

    /// Checks every claim (verify, belongs to space, chain within contiguous runs,
    /// no duplicate seq), every blob hash, manifest genesis presence.
    pub fn verify(&self) -> Result<VerifyReport> {
        todo!()
    }
}

/// Writes a tar bundle: `bundle.json`, then `log/<actor>/<segment>.jsonl` containing
/// exactly `claims` (grouped per actor, ascending seq), then `blobs/<..>/<id>` for `blobs`.
/// Selecting which claims/blobs is the caller's job.
pub fn write_bundle(replica: &Replica, out: &Path, meta: &BundleMeta, claims: &[Claim], blobs: &[Id]) -> Result<()> {
    let _ = (replica, out, meta, claims, blobs);
    todo!()
}

/// Imports a bundle into `dest` (created via `create_replica` if absent; if present its
/// manifest must match the bundle's space). Verifies all claim signatures and blob hashes
/// before writing anything.
pub fn import_bundle(bundle: &Path, dest: &Path) -> Result<(Replica, BundleMeta, ImportReport)> {
    let _ = (bundle, dest);
    todo!()
}
