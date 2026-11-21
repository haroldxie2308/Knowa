//! One replica of a space (DESIGN §5): manifest, per-actor append-only logs and their
//! locks, content-addressed blobs, the rebuild generation, verification and bundles.
//! Storage sits behind two private stores (`backend`), one for the logs and one for
//! the blobs, each at a `Location`: a directory (`dir`) or an object store (`object`).
//! The store knows nothing about status, visibility or queries.

mod backend;
mod blob;
mod bundle;
mod dir;
mod fsutil;
mod location;
mod log;
mod object;
#[cfg(test)]
mod tests;
mod verify;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};

use knowa_model::vocab::{FORMAT_VERSION, OWNER};
use knowa_model::{Claim, Error, ErrorCode, Id, IdKind, Result, Value};
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::{Blobs, Guard, Logs};
use crate::dir::{DirBlobs, DirLogs};
use crate::object::{ObjectBlobs, ObjectLogs, Remote};

pub use crate::location::Location;

pub const MANIFEST_FILE: &str = "knowa.json";
pub const LOG_DIR: &str = "log";
pub const BLOB_DIR: &str = "blobs";
/// `log/<actor>/.lock` in a directory replica: the actor's advisory lock (DESIGN §5.3).
pub const LOCK_FILE: &str = ".lock";
/// The rebuild generation at the replica's location (DESIGN §5.4): a ULID line, absent
/// until the first merge.
pub const GENERATION_FILE: &str = "generation";
pub const SEGMENT_LINES: u64 = 10_000;

/// Characters of `Replica::location_key` (DESIGN §5.2).
const LOCATION_KEY_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub space: Id,
    pub owner: Id,
    pub genesis: Id,
}

/// Exclusive lock on one actor's log (DESIGN §5.3), from `Replica::lock_actor`.
/// Released on drop.
///
/// Not `Send`: the replica records the thread that took it and lets that thread read
/// the actor's log without taking the lock again. On a directory a second lock from
/// the same process waits for this one, so moving the lock to another thread would let
/// the first one deadlock against it.
pub struct ActorLock {
    actor: Id,
    held: Arc<Held>,
    _guard: Guard,
    _thread_bound: PhantomData<*const ()>,
}

impl ActorLock {
    pub fn actor(&self) -> &Id {
        &self.actor
    }
}

impl Drop for ActorLock {
    fn drop(&mut self) {
        // Forgotten before `_guard` releases the lock (fields drop after this body), so
        // no read through the replica skips a lock that is no longer held.
        self.held.release(&self.actor);
    }
}

/// The actor locks a replica holds, each with the thread holding it.
#[derive(Default)]
struct Held(Mutex<BTreeMap<Id, ThreadId>>);

impl Held {
    fn map(&self) -> MutexGuard<'_, BTreeMap<Id, ThreadId>> {
        // Fallback: a panic while the map is locked would poison it, but every update
        // is a single insert or remove, so the map is consistent whatever panicked.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn by_this_thread(&self, actor: &Id) -> bool {
        self.map().get(actor) == Some(&thread::current().id())
    }

    fn hold(&self, actor: &Id) {
        self.map().insert(actor.clone(), thread::current().id());
    }

    fn release(&self, actor: &Id) {
        self.map().remove(actor);
    }
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

/// A replica's two stores, connected but not yet read (DESIGN §5): the logs at the
/// replica's location, the blobs there or at a separate blob location.
pub(crate) struct Stores {
    pub(crate) location: Location,
    pub(crate) logs: Box<dyn Logs>,
    pub(crate) blobs: Box<dyn Blobs>,
}

impl Stores {
    /// The stores of a replica at `location` whose blobs are at `blobs`, or at
    /// `location` when `None`. Object-store blobs pass through `cache`, this machine's
    /// blob cache. An object location's client is configured from the environment.
    fn connect(location: &Location, blobs: Option<&Location>, cache: &Path) -> Result<Stores> {
        let (logs, remote): (Box<dyn Logs>, _) = match location {
            Location::Dir(dir) => (Box::new(DirLogs::new(dir)), None),
            Location::Object(url) => {
                let remote = Remote::s3(url)?;
                (Box::new(ObjectLogs::new(remote.clone())), Some(remote))
            }
        };
        let blobs: Box<dyn Blobs> = match blobs.unwrap_or(location) {
            Location::Dir(dir) => Box::new(DirBlobs::new(dir)),
            Location::Object(url) => {
                // Blobs at the replica's own object location share its client.
                let remote = match remote {
                    Some(remote) if blobs.is_none() => remote,
                    _ => Remote::s3(url)?,
                };
                Box::new(ObjectBlobs::new(remote, cache))
            }
        };
        Ok(Stores { location: location.clone(), logs, blobs })
    }

    /// A directory replica, logs and blobs both in `dir`: a bundle import's target,
    /// which needs no blob cache.
    pub(crate) fn dir(dir: &Path) -> Stores {
        Stores {
            location: Location::Dir(dir.to_path_buf()),
            logs: Box::new(DirLogs::new(dir)),
            blobs: Box::new(DirBlobs::new(dir)),
        }
    }

    /// A replica on `store` at `url`, logs and blobs both there.
    pub(crate) fn object(store: Arc<dyn ObjectStore>, url: &str, cache: &Path) -> Result<Stores> {
        let remote = Remote::new(store, url)?;
        Ok(Stores {
            location: Location::Object(remote.url().to_string()),
            logs: Box::new(ObjectLogs::new(remote.clone())),
            blobs: Box::new(ObjectBlobs::new(remote, cache)),
        })
    }
}

pub struct Replica {
    location: Location,
    manifest: Manifest,
    logs: Box<dyn Logs>,
    blobs: Box<dyn Blobs>,
    held: Arc<Held>,
}

impl Replica {
    /// Creates a new replica at `location` (a directory that does not exist or is
    /// empty, or an object location holding nothing), with its blobs at `blobs`, or at
    /// `location` when `None`; `cache` is this machine's blob cache,
    /// `$KNOWA_HOME/cache/blobs/` (DESIGN §5.2). Appends `genesis` (seq 1,
    /// `knowa:owner`, signed by the owner), then writes the manifest. `genesis` must
    /// verify; `manifest.genesis == genesis.id`.
    pub fn create(location: &Location, blobs: Option<&Location>, cache: &Path, genesis: &Claim) -> Result<Replica> {
        let manifest = genesis_manifest(genesis)?;
        Replica::create_in(Stores::connect(location, blobs, cache)?, manifest, genesis)
    }

    fn create_in(stores: Stores, manifest: Manifest, genesis: &Claim) -> Result<Replica> {
        stores.logs.prepare()?;
        stores.blobs.prepare()?;
        let owner = manifest.owner.clone();
        let replica = Replica::with_stores(stores, manifest);
        let lock = replica.lock_actor(&owner)?;
        replica.append(&lock, std::slice::from_ref(genesis))?;
        drop(lock);
        // The manifest goes last: a location with `knowa.json` is a complete replica.
        replica.write_manifest()?;
        Ok(replica)
    }

    /// Opens an existing replica at `location`, with `blobs` and `cache` as in
    /// `create`; missing/invalid manifest → `not_found`/`integrity`. Credentials an
    /// object store refuses → `usage`.
    pub fn open(location: &Location, blobs: Option<&Location>, cache: &Path) -> Result<Replica> {
        Replica::open_in(Stores::connect(location, blobs, cache)?)
    }

    pub(crate) fn open_in(stores: Stores) -> Result<Replica> {
        let name = format!("{}/{MANIFEST_FILE}", stores.logs.describe());
        let Some(bytes) = stores.logs.read_manifest()? else {
            return Err(Error::not_found(format!(
                "{} is not a Knowa replica (no {MANIFEST_FILE})",
                stores.logs.describe()
            )));
        };
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::integrity(format!("{name}: {e}")))?;
        if manifest.format != FORMAT_VERSION {
            return Err(Error::unsupported(format!(
                "{name}: format {} (this build reads format {FORMAT_VERSION})",
                manifest.format
            )));
        }
        if let Some(p) = manifest_kind_problem(&manifest) {
            return Err(Error::integrity(format!("{name}: {p}")));
        }
        Ok(Replica::with_stores(stores, manifest))
    }

    /// Creates an empty replica for an *existing* space (import target); arguments as
    /// in `create`.
    pub fn create_replica(location: &Location, blobs: Option<&Location>, cache: &Path, manifest: &Manifest) -> Result<Replica> {
        check_manifest(manifest)?;
        Replica::create_replica_in(Stores::connect(location, blobs, cache)?, manifest)
    }

    /// `create_replica` on connected stores, for a `manifest` the caller has checked
    /// (`check_manifest`, or a bundle's validated meta).
    pub(crate) fn create_replica_in(stores: Stores, manifest: &Manifest) -> Result<Replica> {
        stores.logs.prepare()?;
        stores.blobs.prepare()?;
        let replica = Replica::with_stores(stores, manifest.clone());
        replica.write_manifest()?;
        Ok(replica)
    }

    /// `create` on an object store given directly instead of configured from the
    /// environment, for tests in other crates: `object_store`'s in-memory store has no
    /// URL, and no test-only scheme belongs in `Location::parse`. Logs and blobs both
    /// live at `url`.
    #[doc(hidden)]
    pub fn create_on(store: Arc<dyn ObjectStore>, url: &str, cache: &Path, genesis: &Claim) -> Result<Replica> {
        let manifest = genesis_manifest(genesis)?;
        Replica::create_in(Stores::object(store, url, cache)?, manifest, genesis)
    }

    /// `open` on an object store given directly; see `create_on`.
    #[doc(hidden)]
    pub fn open_on(store: Arc<dyn ObjectStore>, url: &str, cache: &Path) -> Result<Replica> {
        Replica::open_in(Stores::object(store, url, cache)?)
    }

    pub(crate) fn with_stores(stores: Stores, manifest: Manifest) -> Replica {
        let Stores { location, logs, blobs } = stores;
        Replica { location, manifest, logs, blobs, held: Arc::default() }
    }

    pub(crate) fn logs(&self) -> &dyn Logs {
        &*self.logs
    }

    pub(crate) fn blob_store(&self) -> &dyn Blobs {
        &*self.blobs
    }

    fn write_manifest(&self) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)?;
        bytes.push(b'\n');
        self.logs.write_manifest(&bytes)
    }

    /// The replica's location, as it was given.
    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Whether two writers of one actor can both hold its lock (DESIGN §5.3): `false`
    /// on a directory, where the lock keeps them out; `true` on an object store, which
    /// has no lock, so the one whose `append` comes second gets `conflict` and must
    /// read the head again and rebuild its claims.
    pub fn writers_can_race(&self) -> bool {
        !self.logs.exclusive_locks()
    }

    /// The key of this replica's machine-local state, `$KNOWA_HOME/index/<s_id>/<key>/`
    /// (DESIGN §5.2): the first 16 characters of b32(sha256(canonical location))
    /// (`Location::canonical`). A directory path that is not UTF-8 → `unsupported`.
    pub fn location_key(&self) -> Result<String> {
        Ok(location_key(&self.location.canonical()?))
    }

    /// The rebuild generation (DESIGN §5.4); `None` until the first merge. A file that
    /// is not one ULID line → `integrity`.
    pub fn generation(&self) -> Result<Option<String>> {
        let Some(bytes) = self.logs.read_generation()? else {
            return Ok(None);
        };
        parse_generation(&bytes).map(Some).ok_or_else(|| {
            Error::integrity(format!("{}: {GENERATION_FILE} is not one ULID line", self.logs.describe()))
        })
    }

    /// Writes a fresh generation, atomically.
    fn bump_generation(&self) -> Result<()> {
        // The model mints ULIDs only inside ids: a new entity id is `e_` + a fresh ULID.
        let entity = Id::new_entity()?;
        let ulid = entity
            .as_str()
            .strip_prefix("e_")
            .ok_or_else(|| Error::internal(format!("new entity id {entity} lacks the e_ prefix")))?;
        self.logs.write_generation(format!("{ulid}\n").as_bytes())
    }

    /// Blocks until `actor`'s exclusive lock is held (DESIGN §5.3), then repairs that
    /// actor's torn tail: an unterminated final line in its newest segment is an
    /// unacknowledged crashed append and is truncated. This thread holding the lock
    /// through this replica already → `internal` (waiting would never end).
    pub fn lock_actor(&self, actor: &Id) -> Result<ActorLock> {
        if actor.kind() != IdKind::Actor {
            return Err(Error::invalid(format!("lock_actor: {actor} is not an actor id")));
        }
        if self.held.by_this_thread(actor) {
            return Err(Error::internal(format!(
                "this thread already holds the lock of {actor} on {}",
                self.logs.describe()
            )));
        }
        let guard = self.logs.lock_exclusive(actor)?;
        self.held.hold(actor);
        let lock = ActorLock { actor: actor.clone(), held: Arc::clone(&self.held), _guard: guard, _thread_bound: PhantomData };
        // Only now: the repair is safe only while no append can be in flight.
        self.logs.repair_tail(actor)?;
        Ok(lock)
    }

    /// `actor` exclusively, for checks that need it so (verify): a new lock, or `None`
    /// when this thread already holds it through this replica.
    pub(crate) fn exclusive(&self, actor: &Id) -> Result<Option<ActorLock>> {
        if self.held.by_this_thread(actor) {
            return Ok(None);
        }
        self.lock_actor(actor).map(Some)
    }

    /// A lock taken on another replica protects nothing here: that is a caller bug.
    fn check_lock(&self, lock: &ActorLock) -> Result<()> {
        if !Arc::ptr_eq(&lock.held, &self.held) {
            return Err(Error::internal(format!(
                "the lock of {} was taken on another replica than {}",
                lock.actor,
                self.logs.describe()
            )));
        }
        Ok(())
    }

    /// Last claim of the lock's actor in this replica (highest seq), if any.
    pub fn head(&self, lock: &ActorLock) -> Result<Option<Head>> {
        self.check_lock(lock)?;
        log::tail(&*self.logs, &self.manifest.space, &lock.actor)
    }

    /// Whether `actor`'s log may hold a claim with seq above `seq`: the readers' fast
    /// path (DESIGN §5.4). Peeks, without any lock and without verifying it, at the last
    /// complete line of the newest segment, ignoring an unterminated final line. `true`
    /// whenever it cannot tell; `false` only when no claim above `seq` exists.
    pub fn may_have_after(&self, actor: &Id, seq: u64) -> Result<bool> {
        log::may_have_after(&*self.logs, actor, seq)
    }

    /// Appends locally-authored claims of the lock's actor (any other author →
    /// `internal`). They must continue the actor's chain exactly (seq = head+1,
    /// prev = head id), in order; each must verify. Durable before returning.
    /// Violations → `conflict` (chain) / `integrity` (verify); nothing is written on
    /// error, and a write that fails part-way is truncated back to the pre-append state.
    pub fn append(&self, lock: &ActorLock, claims: &[Claim]) -> Result<()> {
        self.check_lock(lock)?;
        if let Some(c) = claims.iter().find(|c| c.body.actor != lock.actor) {
            return Err(Error::internal(format!(
                "append under the lock of {}: claim {} is by {}",
                lock.actor, c.id, c.body.actor
            )));
        }
        log::append(&*self.logs, &self.manifest.space, &lock.actor, claims)
    }

    /// Merges foreign claims (bundle import). Each must verify and belong to this space.
    /// Already-present (actor, seq) with the same id → skipped; same (actor, seq) with a
    /// different id → `integrity` (equivocation). Gaps are allowed and reported.
    /// Where seq n-1 is present, `prev` must equal its id → else `integrity`.
    /// Takes the lock of every actor in `claims`, in ascending id order; if anything is
    /// new, writes a fresh rebuild generation before the first segment and again after
    /// the last (DESIGN §5.4).
    pub fn merge(&self, claims: &[Claim]) -> Result<ImportReport> {
        for c in claims {
            log::check_claim(c, &self.manifest.space, ErrorCode::Integrity)?;
        }
        self.merge_checked(claims, false, || Ok(0))
    }

    /// The merge shared with bundle import, over checked `claims`. When anything will
    /// be written (a segment, or blobs: `adds_blobs`), the generation is written first,
    /// then `put_blobs` runs, then the segments are written, then the generation again.
    pub(crate) fn merge_checked(
        &self,
        claims: &[Claim],
        adds_blobs: bool,
        put_blobs: impl FnOnce() -> Result<u64>,
    ) -> Result<ImportReport> {
        let actors: BTreeSet<&Id> = claims.iter().map(|c| &c.body.actor).collect();
        let _locks = actors.into_iter().map(|a| self.lock_actor(a)).collect::<Result<Vec<_>>>()?;
        let space = &self.manifest.space;
        let plan = log::plan_merge(claims, |actor| log::chain(&*self.logs, space, actor))?;
        if !plan.writes_segments() && !adds_blobs {
            return plan.apply(&*self.logs);
        }
        self.bump_generation()?;
        let written = put_blobs().and_then(|blobs_added| {
            let mut report = plan.apply(&*self.logs)?;
            report.blobs_added = blobs_added;
            Ok(report)
        });
        // Also when a write failed part-way: what was written stays, and an index built
        // since the first generation (blobs are read without a lock) may have missed it.
        let after = self.bump_generation();
        match (written, after) {
            (Ok(report), Ok(())) => Ok(report),
            (Ok(_), Err(e)) | (Err(e), Ok(())) => Err(e),
            (Err(mut e), Err(again)) => {
                e.message =
                    format!("{}; writing the rebuild generation after it also failed: {}", e.message, again.message);
                Err(e)
            }
        }
    }

    /// Actors that have a log.
    pub fn actors(&self) -> Result<Vec<Id>> {
        let (actors, problems) = self.logs.actors()?;
        log::fail_on(problems)?;
        Ok(actors)
    }

    /// Claims of `actor` with seq ≥ `from_seq`, ascending by seq. Every claim is
    /// verified (id + signature, `Claim::verify`) before it is yielded: logs are never
    /// trusted (DESIGN §5.3). Failure → `integrity`.
    ///
    /// Unless this thread holds the actor's lock through this replica, the iterator
    /// holds the actor's shared lock until it is dropped (DESIGN §5.3): it waits for an
    /// append in flight and never sees a batch a failed write will take back. It ends
    /// before an unterminated final line, an append that never returned.
    pub fn read_log(&self, actor: &Id, from_seq: u64) -> Result<Box<dyn Iterator<Item = Result<Claim>> + '_>> {
        let guard = if self.held.by_this_thread(actor) {
            None
        } else {
            match self.logs.lock_shared(actor)? {
                Some(guard) => Some(guard),
                None => return Ok(Box::new(std::iter::empty())),
            }
        };
        Ok(Box::new(log::LogIter::new(&*self.logs, &self.manifest.space, actor, from_seq, guard)?))
    }

    /// Streams `r` into the blob store, hashed while staged (DESIGN §5.3 Blob path).
    /// Returns (id, created, size).
    pub fn put_blob(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
        self.blobs.put(r)
    }

    /// Stores the file at `path` (DESIGN §5.3 Blob path). Same result as `put_blob`:
    /// (id, created, size).
    pub fn put_blob_file(&self, path: &Path) -> Result<(Id, bool, u64)> {
        self.blobs.put_file(path)
    }

    /// Accepts a `b_` id with or without fragment; fragment ignored. Another kind of
    /// id → `invalid`.
    pub fn has_blob(&self, id: &Id) -> Result<bool> {
        self.blobs.has(id)
    }

    /// Size of a stored blob; absent → `not_found`.
    pub fn blob_len(&self, id: &Id) -> Result<u64> {
        self.blobs.len(id)
    }

    /// The bytes of a stored blob; absent → `not_found`.
    pub fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.blobs
            .open(id)?
            .read_to_end(&mut bytes)
            .map_err(|e| Error::internal(format!("io: blob {}: {e}", id.base())))?;
        Ok(bytes)
    }

    /// A reader over a stored blob's bytes; absent → `not_found`.
    pub fn open_blob(&self, id: &Id) -> Result<Box<dyn Read + Send>> {
        self.blobs.open(id)
    }

    /// A local file holding a stored blob's bytes, for readers that need a path
    /// (extractors); absent → `not_found`. For a directory replica it is the stored
    /// file itself: callers must not modify it, or the blob no longer matches its id.
    pub fn blob_file(&self, id: &Id) -> Result<PathBuf> {
        self.blobs.local_file(id)
    }

    pub fn blob_ids(&self) -> Result<Vec<Id>> {
        let (blobs, problems) = self.blobs.scan()?;
        log::fail_on(problems)?;
        Ok(blobs.into_iter().map(|(id, _)| id).collect())
    }

    /// Checks every claim (verify, belongs to space, chain within contiguous runs,
    /// no duplicate seq), every blob hash, manifest genesis presence. Each actor is
    /// checked under its exclusive lock (see `verify::verify`).
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

/// The manifest a replica created with `genesis` gets, once `genesis` is checked: it
/// must verify and be a genesis claim (`invalid` otherwise).
fn genesis_manifest(genesis: &Claim) -> Result<Manifest> {
    genesis.verify()?;
    if let Some(p) = genesis_problem(genesis) {
        return Err(Error::invalid(p));
    }
    let b = &genesis.body;
    Ok(Manifest { format: FORMAT_VERSION, space: b.space.clone(), owner: b.actor.clone(), genesis: genesis.id.clone() })
}

/// A manifest given for a new replica must be of this format and name ids of the
/// right kinds (`invalid` otherwise).
fn check_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.format != FORMAT_VERSION {
        return Err(Error::invalid(format!("manifest format {} (expected {FORMAT_VERSION})", manifest.format)));
    }
    if let Some(p) = manifest_kind_problem(manifest) {
        return Err(Error::invalid(format!("manifest: {p}")));
    }
    Ok(())
}

fn manifest_kind_problem(m: &Manifest) -> Option<String> {
    [("space", &m.space, IdKind::Space), ("owner", &m.owner, IdKind::Actor), ("genesis", &m.genesis, IdKind::Claim)]
        .into_iter()
        .find(|(_, id, kind)| id.kind() != *kind)
        .map(|(field, id, kind)| format!("{field} {id} is not a {kind:?} id"))
}

/// A generation file's content: one ULID (26 lowercase base32 characters) and `\n`.
fn parse_generation(bytes: &[u8]) -> Option<String> {
    let ulid = std::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
    (ulid.len() == 26 && ulid.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())).then(|| ulid.to_string())
}

/// The first `LOCATION_KEY_LEN` characters of b32(sha256(`location`)): RFC 4648 base32,
/// lowercase, unpadded, as ids use it. knowa-model keeps its encoder private, so this
/// encodes the 80 bits the key needs (the digest's first 10 bytes) directly.
fn location_key(location: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(location.as_bytes());
    let bits = digest[..10].iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    (0..LOCATION_KEY_LEN).rev().map(|i| char::from(ALPHABET[((bits >> (5 * i)) & 0x1f) as usize])).collect()
}

/// Hashes the file at `path` in place, without storing it (DESIGN §5.3 Catalog-only
/// blobs). Missing → not_found; not a regular file → invalid.
pub fn hash_file(path: &Path) -> Result<(Id, u64)> {
    blob::hash_file(path)
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

/// Imports a bundle into the directory `dest`, a replica whose blobs live in it too
/// (created via `create_replica` if absent; if present its manifest must match the
/// bundle's space). Verifies all claim signatures and blob hashes,
/// and that the bundle's genesis is a genesis claim, before writing anything. Merges as
/// `Replica::merge` does, and also counts the blobs it adds as writes: the rebuild
/// generation is written before the first blob or segment it adds and after the last,
/// because a text blob arriving for claims that are already indexed changes their
/// full-text documents, which catch-up never revisits.
pub fn import_bundle(bundle: &Path, dest: &Path) -> Result<(Replica, BundleMeta, ImportReport)> {
    bundle::import(bundle, dest)
}
