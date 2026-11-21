//! Derived index of ONE replica (DESIGN §4.7, §5, §8). Everything here can be
//! deleted and rebuilt from the replica's logs and blobs. Multi-space composition
//! (fan-out, merge, context, path) lives in `knowa-api`, not here.

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

use std::path::{Path, PathBuf};
use std::time::Duration;

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorInfo, Claim, ClaimView, Edge, EntityView, Error, Id, Result, Visibility};
use knowa_store::Replica;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::blobs::BlobSource;
use crate::db::{INDEX_FILE, META_EPOCH, META_SCHEMA, META_SPACE, db};

/// Bump whenever the SQLite schema or a rule that derives stored values (statuses,
/// entity summaries) changes; a mismatch triggers a rebuild.
pub const SCHEMA_VERSION: u32 = 3;

/// Text blobs up to this size are full-text indexed (DESIGN §7.2).
pub const MAX_FTS_BLOB_BYTES: u64 = 1024 * 1024;

/// Claims handed to one `apply` while rebuilding or catching up, bounding memory.
const REPLAY_CHUNK: usize = 5000;

/// Another process may hold the write lock (e.g. a concurrent `knowa write`).
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Page<T> {
    pub items: Vec<T>,
    /// opaque; None when exhausted
    pub cursor: Option<String>,
}

pub struct Index {
    conn: Connection,
    epoch: String,
    space: Id,
    /// genesis owner, from the manifest
    owner: Id,
    /// replica root, for operations that need the blob store
    root: PathBuf,
}

impl Index {
    /// Opens `<replica>/.index/index.sqlite`. Missing file, schema mismatch or a
    /// rebuild marker (`<replica>/.index/<knowa_store::REBUILD_MARKER>`, left by
    /// `merge`) → full rebuild (new epoch). Otherwise catches up: for each actor,
    /// applies log claims with seq above the highest indexed seq for that actor; claims
    /// filling gaps below it are what the marker is for (DESIGN §5). Callers hold the
    /// replica's write lock, as `merge` does, so no marker appears mid-rebuild.
    pub fn open(replica: &Replica) -> Result<Index> {
        let dir = replica.index_dir()?;
        if dir.join(knowa_store::REBUILD_MARKER).try_exists()? {
            return Index::rebuild(replica);
        }
        let m = replica.manifest();
        match Index::open_existing(&dir.join(INDEX_FILE), &m.space, &m.owner, replica.root())? {
            Some(mut index) => {
                index.catch_up(replica)?;
                Ok(index)
            }
            None => Index::rebuild(replica),
        }
    }

    /// Deletes and rebuilds from the logs. New epoch. Once the rebuild is complete the
    /// rebuild marker, if any, has served its purpose and is removed.
    pub fn rebuild(replica: &Replica) -> Result<Index> {
        let dir = replica.index_dir()?;
        let m = replica.manifest();
        let mut index = Index::create(&dir.join(INDEX_FILE), &m.space, &m.owner, replica.root())?;
        index.catch_up(replica)?;
        index.mark_complete()?;
        match std::fs::remove_file(dir.join(knowa_store::REBUILD_MARKER)) {
            Ok(()) => {}
            // the common case: nothing was merged since the last build
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(index)
    }

    /// Incrementally applies claims just appended/merged into `replica` (any order),
    /// assigns arrival numbers, recomputes status/stale for every affected claim
    /// (targets of review/retract/replaces, dependents via src, and — on policy
    /// changes — all claims of the affected actor), refreshes affected entity rows,
    /// per-claim FTS docs (incl. text blobs read from `replica`) and geo.
    pub fn apply(&mut self, replica: &Replica, claims: &[knowa_model::Claim]) -> Result<()> {
        self.apply_with(replica, claims)
    }

    /// Random token chosen at (re)build time.
    pub fn epoch(&self) -> &str {
        &self.epoch
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

    /// Claims in local arrival order after `after` (arrival number), up to `limit`.
    /// Returns (claims, last arrival number). Caller formats `<epoch>:<n>`.
    pub fn feed(&self, after: u64, limit: u32) -> Result<(Vec<ClaimView>, u64)> {
        read::feed(&self.conn, after, limit)
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
    pub fn export_set(&self, level: Visibility) -> Result<(Vec<Id>, Vec<Id>)> {
        let replica = self.replica()?;
        self.export_set_with(level, &replica)
    }

    /// Blobs present in the replica that have no claim with `via.worker == worker`
    /// (e.g. "exif@1"), for `knowa extract`.
    pub fn blobs_without_worker(&self, worker: &str) -> Result<Vec<Id>> {
        let replica = self.replica()?;
        self.blobs_without_worker_with(worker, &replica)
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

    fn export_set_with(&self, level: Visibility, blobs: &dyn BlobSource) -> Result<(Vec<Id>, Vec<Id>)> {
        export::export_set(&self.conn, &self.space, level, blobs)
    }

    fn blobs_without_worker_with(&self, worker: &str, blobs: &dyn BlobSource) -> Result<Vec<Id>> {
        let done = read::worker_subjects(&self.conn, worker)?;
        let mut out: Vec<Id> = blobs.blob_ids()?.into_iter().filter(|b| !done.contains(b)).collect();
        out.sort();
        Ok(out)
    }

    /// The blob store of the replica this index was opened for. `export_set` and
    /// `blobs_without_worker` take no replica argument, so they reopen it by path.
    fn replica(&self) -> Result<Replica> {
        Replica::open(&self.root)
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

    /// A fresh, empty index at `path` (old files removed) with a new epoch. The schema
    /// version is written only once the caller has filled it, so a crash mid-build
    /// leaves a file that `open` rebuilds.
    fn create(path: &Path, space: &Id, owner: &Id, root: &Path) -> Result<Index> {
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
        let index = Index { conn, epoch, space: space.clone(), owner: owner.clone(), root: root.to_path_buf() };
        index.set_meta(META_EPOCH, &index.epoch)?;
        index.set_meta(META_SPACE, space.as_str())?;
        apply::init_policy(&index.conn, owner)?;
        Ok(index)
    }

    /// The index at `path` if it exists, is complete, has this schema version and
    /// belongs to `space`; `None` means it must be rebuilt.
    fn open_existing(path: &Path, space: &Id, owner: &Id, root: &Path) -> Result<Option<Index>> {
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
        let epoch = get(META_EPOCH)?.ok_or_else(|| db::corrupt("complete index without an epoch"))?;
        Ok(Some(Index { conn, epoch, space: space.clone(), owner: owner.clone(), root: root.to_path_buf() }))
    }

    /// Written last by a (re)build: from now on `open` trusts this file.
    fn mark_complete(&self) -> Result<()> {
        self.set_meta(META_SCHEMA, &SCHEMA_VERSION.to_string())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value])
            .map_err(db)?;
        Ok(())
    }

    /// Applies, per actor in id order and seq ascending, every log claim above the
    /// actor's highest indexed seq.
    fn catch_up(&mut self, replica: &Replica) -> Result<()> {
        let heads: std::collections::HashMap<Id, u64> = self.actor_heads()?.into_iter().collect();
        let mut actors = replica.actors()?;
        actors.sort();
        for actor in actors {
            let from = heads.get(&actor).map_or(1, |h| h + 1);
            let mut chunk = Vec::with_capacity(REPLAY_CHUNK);
            for claim in replica.read_log(&actor, from)? {
                chunk.push(claim?);
                if chunk.len() == REPLAY_CHUNK {
                    self.apply_with(replica, &chunk)?;
                    chunk.clear();
                }
            }
            if !chunk.is_empty() {
                self.apply_with(replica, &chunk)?;
            }
        }
        Ok(())
    }
}
