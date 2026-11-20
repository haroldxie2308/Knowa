//! Derived index of ONE replica (DESIGN §4.7, §5, §8). Everything here can be
//! deleted and rebuilt from the replica's logs and blobs. Multi-space composition
//! (fan-out, merge, context, path) lives in `knowa-api`, not here.

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorInfo, ClaimView, Edge, EntityView, Id, Result, Visibility};
use knowa_store::Replica;

/// Bump whenever the SQLite schema changes; a mismatch triggers a rebuild.
pub const SCHEMA_VERSION: u32 = 1;

/// Text blobs up to this size are full-text indexed (DESIGN §7.2).
pub const MAX_FTS_BLOB_BYTES: u64 = 1024 * 1024;

pub struct Page<T> {
    pub items: Vec<T>,
    /// opaque; None when exhausted
    pub cursor: Option<String>,
}

pub struct Index {
    // rusqlite::Connection + epoch + whatever the implementation needs
    _private: (),
}

impl Index {
    /// Opens `<replica>/.index/index.sqlite`. Missing file or schema mismatch → full
    /// rebuild (new epoch). Otherwise catches up: for each actor, applies log claims with
    /// seq above the highest indexed seq for that actor (handles gaps from merges).
    pub fn open(replica: &Replica) -> Result<Index> {
        let _ = replica;
        todo!()
    }

    /// Deletes and rebuilds from the logs. New epoch.
    pub fn rebuild(replica: &Replica) -> Result<Index> {
        let _ = replica;
        todo!()
    }

    /// Incrementally applies claims just appended/merged into `replica` (any order),
    /// assigns arrival numbers, recomputes status/stale for every affected claim
    /// (targets of review/retract/replaces, dependents via src, and — on policy
    /// changes — all claims of the affected actor), refreshes affected entity rows,
    /// FTS docs (incl. text blobs read from `replica`), geo and edges.
    pub fn apply(&mut self, replica: &Replica, claims: &[knowa_model::Claim]) -> Result<()> {
        let _ = (replica, claims);
        todo!()
    }

    /// Random token chosen at (re)build time.
    pub fn epoch(&self) -> &str {
        todo!()
    }

    /// Highest indexed seq per actor (for catch-up / diagnostics).
    pub fn actor_heads(&self) -> Result<Vec<(Id, u64)>> {
        todo!()
    }

    /// Non-retracted claim by `actor` with this equivalence key (DESIGN §4.4).
    pub fn equivalent(&self, actor: &Id, equiv_key: &str) -> Result<Option<ClaimView>> {
        let _ = (actor, equiv_key);
        todo!()
    }

    /// Claims by id, in the given order; unknown ids are omitted.
    pub fn claims(&self, ids: &[Id]) -> Result<Vec<ClaimView>> {
        let _ = ids;
        todo!()
    }

    /// `where.near_id` must already be resolved to `near` (else `invalid`).
    /// `cursor` is this index's own opaque cursor.
    pub fn find_claims(&self, w: &Where, view: &View, order: Order, limit: u32, cursor: Option<&str>) -> Result<Page<ClaimView>> {
        let _ = (w, view, order, limit, cursor);
        todo!()
    }

    /// As `find_claims`, returning entities (see `Where` docs for matching semantics).
    /// `score` set for text queries (higher = better); `distance_km` set when `near`.
    pub fn find_entities(&self, w: &Where, view: &View, order: Order, limit: u32, cursor: Option<&str>) -> Result<Page<EntityView>> {
        let _ = (w, view, order, limit, cursor);
        todo!()
    }

    /// Summary of one entity if any claim about it passes `view`; `spaces` = [this space].
    pub fn entity(&self, id: &Id, view: &View) -> Result<Option<EntityView>> {
        let _ = (id, view);
        todo!()
    }

    /// Claims with s = id (`Dir::Out`) and with o.ref = id (`Dir::In`, base id match,
    /// fragments ignored), passing `view`, ordered by `at` ascending.
    pub fn claims_about(&self, id: &Id, view: &View, limit: u32) -> Result<(Vec<ClaimView>, Vec<ClaimView>)> {
        let _ = (id, view, limit);
        todo!()
    }

    /// Ref-valued edges touching `id` (both directions) passing `view`, excluding
    /// policy predicates.
    pub fn edges(&self, id: &Id, view: &View) -> Result<Vec<Edge>> {
        let _ = (id, view);
        todo!()
    }

    /// Claims in local arrival order after `after` (arrival number), up to `limit`.
    /// Returns (claims, last arrival number). Caller formats `<epoch>:<n>`.
    pub fn feed(&self, after: u64, limit: u32) -> Result<(Vec<ClaimView>, u64)> {
        let _ = (after, limit);
        todo!()
    }

    /// Raw audit log by record time ascending, regardless of status.
    pub fn log(&self, actors: &[Id], since_ms: Option<i64>, limit: u32, cursor: Option<&str>) -> Result<Page<ClaimView>> {
        let _ = (actors, since_ms, limit, cursor);
        todo!()
    }

    /// Every actor that authored a claim or holds a role here.
    pub fn actors(&self) -> Result<Vec<ActorInfo>> {
        todo!()
    }

    /// Claims and blobs to export at `level` (DESIGN §7.3): claims whose effective
    /// visibility ≥ level, plus ALL policy claims, plus every blob referenced (s, o.ref,
    /// src, q refs) by an exported claim and present in the replica.
    /// `Visibility::Private` ⇒ everything.
    pub fn export_set(&self, level: Visibility) -> Result<(Vec<Id>, Vec<Id>)> {
        let _ = level;
        todo!()
    }

    /// Blobs present in the replica that have no claim with `via.worker == worker`
    /// (e.g. "exif@1"), for `knowa extract`.
    pub fn blobs_without_worker(&self, worker: &str) -> Result<Vec<Id>> {
        let _ = worker;
        todo!()
    }

    /// Accepted-or-proposed `knowa:mediaType` of a blob, if known.
    pub fn media_type(&self, blob: &Id) -> Result<Option<String>> {
        let _ = blob;
        todo!()
    }
}
