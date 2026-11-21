//! Derived index of ONE replica (DESIGN §4.7, §5, §8). Everything here can be
//! deleted and rebuilt from the replica's logs and blobs. It is machine-local: the
//! caller chooses its directory (DESIGN §5.2), and nothing is stored in the replica.
//! Multi-space composition (fan-out, merge, context, path) lives in `knowa-api`.

mod apply;
mod blobs;
mod db;
mod entity;
mod export;
mod fts;
mod geo;
mod policy;
mod read;
mod rules;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorInfo, Claim, ClaimView, Edge, EntityView, Error, Id, IdKind, Result, Visibility};
use knowa_store::Replica;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::blobs::BlobSource;
use crate::db::{INDEX_FILE, META_EPOCH, META_GENERATION, META_SCHEMA, META_SPACE, db};

/// Bump whenever the SQLite schema, its meta keys or a rule that derives stored values
/// (statuses, entity summaries, full-text documents) changes; a mismatch triggers a
/// rebuild.
pub const SCHEMA_VERSION: u32 = 7;

/// `<index dir>/.lock`: the index lock (see `IndexLock`).
pub const INDEX_LOCK_FILE: &str = ".lock";

/// Text blobs up to this size are full-text indexed (DESIGN §7.2).
pub const MAX_FTS_BLOB_BYTES: u64 = 1024 * 1024;

/// Claims handed to one `apply` while rebuilding or catching up, bounding memory.
const REPLAY_CHUNK: usize = 5000;

/// Another process's connection may briefly hold SQLite's own locks (a WAL checkpoint,
/// opening the file); writers are serialized by the index lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The index lock (DESIGN §5.2, §5.3): an exclusive advisory lock on
/// `<index dir>/.lock`, held for one reset of a rebuild, one catch-up apply, one apply
/// or one blob refresh, and released at once. Whoever holds it waits for nothing but
/// SQLite, never for an actor lock, so a writer holding its actor locks may always
/// take it without risking a deadlock.
struct IndexLock {
    _file: File,
}

impl IndexLock {
    fn take(dir: &Path) -> Result<IndexLock> {
        let path = dir.join(INDEX_LOCK_FILE);
        let io = |e: std::io::Error| Error::internal(format!("io: {}: {e}", path.display()));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(io)?;
        file.lock().map_err(io)?;
        Ok(IndexLock { _file: file })
    }
}

pub struct Page<T> {
    pub items: Vec<T>,
    /// opaque; None when exhausted
    pub cursor: Option<String>,
}

pub struct Index {
    conn: Connection,
    epoch: String,
    /// the replica's rebuild generation this index was built against (DESIGN §5.4)
    generation: Option<String>,
    space: Id,
    /// genesis owner, from the manifest
    owner: Id,
    /// the index directory, which holds the index lock
    dir: PathBuf,
}

impl Index {
    /// Opens the index of `replica` kept in `dir` (created if missing) and catches it
    /// up with every actor's log (`catch_up`). Rebuilt first, with a new epoch, when
    /// it is missing, has another schema version or space, or was built against
    /// another rebuild generation than the replica's (DESIGN §5.4).
    pub fn open(replica: &Replica, dir: &Path) -> Result<Index> {
        let mut index = Index::load(replica, dir)?;
        index.catch_up(replica)?;
        Ok(index)
    }

    /// As `open`, but without the catch-up unless it rebuilds: for a writer, which
    /// then catches up only the actors it locks (`catch_up_actors`). A rebuild reads
    /// every actor's log, waiting for any append in flight, so the caller must hold
    /// no actor lock (DESIGN §5.3: writers for different actors never wait for each
    /// other).
    pub fn load(replica: &Replica, dir: &Path) -> Result<Index> {
        std::fs::create_dir_all(dir).map_err(|e| Error::internal(format!("io: {}: {e}", dir.display())))?;
        // Read before the index is checked or built: a merge that starts after this
        // read writes a new generation, and the next open rebuilds again.
        let generation = replica.generation()?;
        let m = replica.manifest();
        match Index::open_existing(&dir.join(INDEX_FILE), dir, &m.space, &m.owner, generation.as_deref())? {
            Some(index) => Ok(index),
            None => Index::rebuild_from(replica, dir, generation, false),
        }
    }

    /// Deletes and rebuilds from the logs (`knowa reindex`). New epoch. Like `load`,
    /// the caller must hold no actor lock.
    pub fn rebuild(replica: &Replica, dir: &Path) -> Result<Index> {
        std::fs::create_dir_all(dir).map_err(|e| Error::internal(format!("io: {}: {e}", dir.display())))?;
        let generation = replica.generation()?;
        Index::rebuild_from(replica, dir, generation, true)
    }

    /// Under the index lock, replaces the index with an empty one recorded as built
    /// against `generation` — unless `force` is off and another process rebuilt it for
    /// that generation while this one waited for the lock — then fills it by catch-up,
    /// which reads the logs without holding the index lock (DESIGN §5.3 lock order). A
    /// crash part-way leaves an index that the next catch-up completes: catch-up
    /// resumes per actor from the highest seq applied.
    fn rebuild_from(replica: &Replica, dir: &Path, generation: Option<String>, force: bool) -> Result<Index> {
        let m = replica.manifest();
        let path = dir.join(INDEX_FILE);
        let mut index = {
            let _lock = IndexLock::take(dir)?;
            let current = if force {
                None
            } else {
                Index::open_existing(&path, dir, &m.space, &m.owner, generation.as_deref())?
            };
            match current {
                Some(index) => index,
                None => {
                    let index = Index::create(&path, dir, &m.space, &m.owner, generation.as_deref())?;
                    index.mark_complete()?;
                    index
                }
            }
        };
        index.catch_up(replica)?;
        Ok(index)
    }

    /// Brings the index up to date with every actor's log (DESIGN §5.4).
    pub fn catch_up(&mut self, replica: &Replica) -> Result<()> {
        let actors = replica.actors()?;
        self.catch_up_actors(replica, &actors)
    }

    /// Applies, per actor in id order, the log claims above the actor's highest indexed
    /// seq (DESIGN §5.4). An actor whose newest complete line is not past that head is
    /// skipped without reading or waiting (`Replica::may_have_after`). Otherwise its
    /// new lines are read in chunks under its shared lock (directly, when this thread
    /// holds its exclusive lock through `replica`), which is released before each
    /// chunk is applied under the index lock, skipping claims at or below the head as
    /// it is then: another process may have applied them meanwhile (DESIGN §5.3).
    /// Claims that fill gaps below an indexed head are not seen; the rebuild
    /// generation covers them.
    pub fn catch_up_actors(&mut self, replica: &Replica, actors: &[Id]) -> Result<()> {
        let heads: HashMap<Id, u64> = self.actor_heads()?.into_iter().collect();
        let mut actors = actors.to_vec();
        actors.sort();
        actors.dedup();
        for actor in &actors {
            let mut from = heads.get(actor).map_or(1, |h| h + 1);
            if !replica.may_have_after(actor, from - 1)? {
                continue;
            }
            loop {
                // The iterator holds the shared lock; it is dropped here, before the index
                // lock is taken.
                let chunk = replica.read_log(actor, from)?.take(REPLAY_CHUNK).collect::<Result<Vec<Claim>>>()?;
                let Some(last) = chunk.last().map(|c| c.body.seq) else {
                    break;
                };
                let full = chunk.len() == REPLAY_CHUNK;
                self.apply_above_head(replica, actor, chunk)?;
                if !full {
                    break;
                }
                from = last + 1;
            }
        }
        Ok(())
    }

    /// Incrementally applies claims just appended into `replica`, assigns arrival
    /// numbers, recomputes status/stale for every affected claim (targets of
    /// review/retract/replaces, dependents via src, and — on policy changes — all
    /// claims of the affected actor), refreshes affected entity rows, per-claim FTS docs
    /// (incl. text blobs read from `replica`) and geo. Under the index lock.
    ///
    /// Each actor's claims must come in ascending seq, above every seq of that actor
    /// already indexed, so that its arrival order is its seq order, which the feed
    /// relies on (`read::feed`). A writer meets this by applying the batch it appended
    /// while still holding the actor's exclusive lock, under which it caught the actor
    /// up first.
    pub fn apply(&mut self, replica: &Replica, claims: &[Claim]) -> Result<()> {
        let _lock = IndexLock::take(&self.dir)?;
        self.apply_with(replica, claims)
    }

    /// Re-derives, in one transaction, the full-text documents that carry `blob`'s text
    /// (its `text/*` `knowa:mediaType` claims and the `knowa:content` claims linking
    /// it) from the replica's blob store. For bytes that arrived without a new claim
    /// about them, which neither `apply` nor catch-up revisits (DESIGN §7.2). A
    /// fragment is ignored; a non-blob id → `invalid`. Under the index lock; needs no
    /// actor lock.
    pub fn refresh_blob(&mut self, replica: &Replica, blob: &Id) -> Result<()> {
        let _lock = IndexLock::take(&self.dir)?;
        self.refresh_blob_with(blob, replica)
    }

    /// Random token chosen at (re)build time.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// The replica's rebuild generation this index was built against (DESIGN §5.4);
    /// `None` for a replica that has none. Feed cursors record it.
    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    /// Highest indexed seq per actor (for catch-up / diagnostics).
    pub fn actor_heads(&self) -> Result<Vec<(Id, u64)>> {
        read::actor_heads(&self.conn)
    }

    /// An active (accepted|proposed), non-stale claim by `actor` with this equivalence
    /// key (DESIGN §4.4); superseded/rejected/retracted/stale equivalents don't count.
    pub fn equivalent(&self, actor: &Id, equiv_key: &str) -> Result<Option<ClaimView>> {
        read::equivalent(&self.conn, actor, equiv_key)
    }

    /// Claims by id, in the given order; unknown ids are omitted.
    pub fn claims(&self, ids: &[Id]) -> Result<Vec<ClaimView>> {
        read::claims(&self.conn, ids)
    }

    /// `where.near_id` must already be resolved to `near` (else `invalid`).
    /// `cursor` is this index's own opaque cursor.
    pub fn find_claims(&self, w: &Where, view: &View, order: Order, limit: u32, cursor: Option<&str>) -> Result<Page<ClaimView>> {
        read::find_claims(&self.conn, w, view, order, limit, cursor)
    }

    /// As `find_claims`, returning entities (see `Where` docs for matching semantics).
    /// Matching on text/type/when/near and every display field use only claims passing
    /// `view` (DESIGN §4.7). `score` set for text queries (higher = better);
    /// `distance_km` set when `near`.
    pub fn find_entities(&self, w: &Where, view: &View, order: Order, limit: u32, cursor: Option<&str>) -> Result<Page<EntityView>> {
        read::find_entities(&self.conn, &self.space, w, view, order, limit, cursor)
    }

    /// Summary of one entity, computed from the claims about it that pass `view`, if
    /// any does; `spaces` = [this space].
    pub fn entity(&self, id: &Id, view: &View) -> Result<Option<EntityView>> {
        read::entity(&self.conn, &self.space, id, view)
    }

    /// Claims with s = id (`Dir::Out`) and with o.ref = id (`Dir::In`, base id match,
    /// fragments ignored), passing `view`, ordered by `at` ascending.
    pub fn claims_about(&self, id: &Id, view: &View, limit: u32) -> Result<(Vec<ClaimView>, Vec<ClaimView>)> {
        read::claims_about(&self.conn, id, view, limit)
    }

    /// Ref-valued edges touching `id` (both directions) passing `view`, excluding
    /// policy predicates.
    pub fn edges(&self, id: &Id, view: &View) -> Result<Vec<Edge>> {
        read::edges(&self.conn, id, view)
    }

    /// DESIGN §6 feed: the claims not yet delivered according to `delivered`, the
    /// highest seq already delivered per actor (an actor without an entry has had
    /// nothing delivered), in local arrival order, at most `limit`. The map is only
    /// meaningful at the `generation` it was built against, which the caller checks;
    /// `read::feed` says why a page then never steps over a claim.
    pub fn feed(&self, delivered: &BTreeMap<Id, u64>, limit: u32) -> Result<Vec<ClaimView>> {
        read::feed(&self.conn, delivered, limit)
    }

    /// Raw audit log by record time ascending, regardless of status.
    pub fn log(&self, actors: &[Id], since_ms: Option<i64>, limit: u32, cursor: Option<&str>) -> Result<Page<ClaimView>> {
        read::log(&self.conn, actors, since_ms, limit, cursor)
    }

    /// Every actor that authored a claim or holds a role here.
    pub fn actors(&self) -> Result<Vec<ActorInfo>> {
        read::actors(&self.conn)
    }

    /// Claims and blobs to export at `level` (DESIGN §7.3). Shared/public: non-policy
    /// claims whose effective visibility ≥ level, trust claims (owner, role,
    /// autoAccept, actorKind, defaultVisibility), and review/retract claims whose
    /// target is exported; never visibility claims. Blobs present in the replica whose
    /// own effective visibility ≥ level. `Visibility::Private` ⇒ every claim and every
    /// referenced (s, o.ref, src, q refs), present blob.
    pub fn export_set(&self, replica: &Replica, level: Visibility) -> Result<(Vec<Id>, Vec<Id>)> {
        self.export_set_with(level, replica)
    }

    /// Blobs present in the replica that have no claim with `via.worker == worker`
    /// (e.g. "exif@1"), for `knowa extract`.
    pub fn blobs_without_worker(&self, replica: &Replica, worker: &str) -> Result<Vec<Id>> {
        self.blobs_without_worker_with(worker, replica)
    }

    /// Accepted-or-proposed `knowa:mediaType` of a blob, if known.
    pub fn media_type(&self, blob: &Id) -> Result<Option<String>> {
        read::media_type(&self.conn, blob)
    }

    // ---- internals ----

    fn apply_with(&mut self, blobs: &dyn BlobSource, claims: &[Claim]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        apply::apply(&tx, &self.space, &self.owner, blobs, claims)?;
        tx.commit().map_err(db)
    }

    /// Applies `actor`'s `claims` that are above its indexed head, read under the index
    /// lock that this takes.
    fn apply_above_head(&mut self, replica: &Replica, actor: &Id, claims: Vec<Claim>) -> Result<()> {
        let _lock = IndexLock::take(&self.dir)?;
        let head = read::actor_head(&self.conn, actor)?;
        let fresh: Vec<Claim> = claims.into_iter().filter(|c| head.is_none_or(|h| c.body.seq > h)).collect();
        if fresh.is_empty() {
            return Ok(());
        }
        self.apply_with(replica, &fresh)
    }

    fn refresh_blob_with(&mut self, blob: &Id, blobs: &dyn BlobSource) -> Result<()> {
        if blob.kind() != IdKind::Blob {
            return Err(Error::invalid(format!("refresh_blob: {blob} is not a blob id")));
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        entity::refresh_blob_text(&tx, blobs, blob.base().as_str())?;
        tx.commit().map_err(db)
    }

    fn export_set_with(&self, level: Visibility, blobs: &dyn BlobSource) -> Result<(Vec<Id>, Vec<Id>)> {
        export::export_set(&self.conn, &self.space, level, blobs)
    }

    fn blobs_without_worker_with(&self, worker: &str, blobs: &dyn BlobSource) -> Result<Vec<Id>> {
        let done = read::worker_subjects(&self.conn, worker)?;
        let mut out: Vec<Id> = blobs.blob_ids()?.into_iter().filter(|b| !done.contains(b)).collect();
        out.sort();
        Ok(out)
    }

    fn connect(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path).map_err(db)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(db)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::internal(format!("index database refused WAL mode (got {mode:?})")));
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL;").map_err(db)?;
        Ok(conn)
    }

    /// A fresh, empty index at `path` (old files removed) with a new epoch, recorded as
    /// built against `generation`. The schema version is written only once the caller
    /// marks it complete, so a crash before that leaves a file that `open` rebuilds.
    /// The caller holds the index lock of `dir`.
    fn create(path: &Path, dir: &Path, space: &Id, owner: &Id, generation: Option<&str>) -> Result<Index> {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let conn = Index::connect(path)?;
        conn.execute_batch(db::SCHEMA).map_err(db)?;
        let epoch: String = conn.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0)).map_err(db)?;
        let index = Index {
            conn,
            epoch,
            generation: generation.map(str::to_string),
            space: space.clone(),
            owner: owner.clone(),
            dir: dir.to_path_buf(),
        };
        index.set_meta(META_EPOCH, &index.epoch)?;
        index.set_meta(META_SPACE, space.as_str())?;
        if let Some(g) = generation {
            index.set_meta(META_GENERATION, g)?;
        }
        apply::init_policy(&index.conn, owner)?;
        Ok(index)
    }

    /// The index at `path` if it exists, is complete, has this schema version, belongs
    /// to `space` and was built against `generation`; `None` means it must be rebuilt.
    fn open_existing(path: &Path, dir: &Path, space: &Id, owner: &Id, generation: Option<&str>) -> Result<Option<Index>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Index::connect(path)?;
        let has_meta: bool = conn
            .query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')", [], |r| r.get(0))
            .map_err(db)?;
        if !has_meta {
            return Ok(None);
        }
        let get = |key: &str| -> Result<Option<String>> {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(db)
        };
        if get(META_SCHEMA)?.as_deref() != Some(SCHEMA_VERSION.to_string().as_str()) {
            return Ok(None);
        }
        // An index copied in from another space's replica is foreign derived data.
        if get(META_SPACE)?.as_deref() != Some(space.as_str()) {
            return Ok(None);
        }
        // A merge since the build may have filled gaps below indexed heads (DESIGN §5.4).
        if get(META_GENERATION)?.as_deref() != generation {
            return Ok(None);
        }
        let epoch = get(META_EPOCH)?.ok_or_else(|| db::corrupt("complete index without an epoch"))?;
        Ok(Some(Index {
            conn,
            epoch,
            generation: generation.map(str::to_string),
            space: space.clone(),
            owner: owner.clone(),
            dir: dir.to_path_buf(),
        }))
    }

    /// Written by `create`'s caller, before the catch-up that fills the index: from
    /// then on `open` trusts this file (catch-up resumes where a crash stopped it).
    fn mark_complete(&self) -> Result<()> {
        self.set_meta(META_SCHEMA, &SCHEMA_VERSION.to_string())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value])
            .map_err(db)?;
        Ok(())
    }
}
