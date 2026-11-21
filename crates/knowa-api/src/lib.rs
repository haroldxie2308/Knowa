//! The stable core (DESIGN §6): five primitives plus admin ops, composed over
//! store + index + extract + config. The CLI is a thin adapter over this crate; a
//! future MCP server or language binding is another adapter over the same calls.

use std::io::Read;
use std::path::{Path, PathBuf};

use knowa_config::Home;
use knowa_model::query::{BlobPutResult, FeedResult, QueryDoc, QueryResult, Where, WriteResult};
use knowa_model::{Draft, Error, ErrorCode, Id, IdKind, Result, Visibility, vocab};
use serde::Serialize;

/// Where a replica or its blobs live (DESIGN §5.2): `init` and `space add` take it.
pub use knowa_store::Location;
pub use select::canonical as canonical_location;

mod admin;
mod context;
mod feed;
mod ingest;
mod merge;
mod path;
mod query;
mod review;
mod schema;
mod select;
#[cfg(test)]
mod test_graph;
mod worker;
mod write;
#[cfg(test)]
mod write_race;

/// Space/actor selectors as given by the caller (flag or env), unresolved.
#[derive(Debug, Clone, Default)]
pub struct Sel {
    pub space: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitResult {
    pub space: Id,
    /// The replica's canonical location (DESIGN §5.2). Its JSON key stays `path`, as
    /// --json output evolves additively (DESIGN §6.4); for a directory it is the same
    /// absolute path as before.
    #[serde(rename = "path")]
    pub location: String,
    /// the canonical blob location, when it is not the replica's
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<String>,
    pub alias: String,
    pub owner: Id,
    /// the built-in extract worker, granted Worker + autoAccept
    pub extract_actor: Id,
    pub claims: Vec<Id>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PutOptions {
    /// skip the built-in extractors
    pub no_extract: bool,
    /// catalog only: hash each file in place and write its claims without copying its
    /// bytes into the replica (DESIGN §5 Catalog-only blobs)
    pub no_store: bool,
    /// read evicted files (macOS dataless), which makes the OS download them first;
    /// without it each is skipped as `unsupported` (DESIGN §7, `put`)
    pub download_evicted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PutItem {
    pub path: PathBuf,
    pub blob: Id,
    pub created: bool,
    /// the replica holds this file's bytes after this item: always true when stored now,
    /// and under no_store true only if an earlier put already stored them
    pub stored: bool,
    pub size: u64,
    /// ids of claims written for this file (by the actor and by the extract worker)
    pub claims: Vec<Id>,
    /// per-file problems that did not stop the run: "<worker>: <message>" for a failed
    /// extractor, or a file time with no EDTF form (its claim is left out)
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PutResult {
    pub items: Vec<PutItem>,
    /// files that failed on their own and were not added; the run went on without them
    pub skipped: Vec<PutSkip>,
}

/// A file `put` did not add because of a failure of that file alone: it changed while
/// being added, vanished, could not be read, has a non-UTF-8 name, is no longer a
/// regular file, or is evicted (DESIGN §7, `put`). `code` and `message` are that
/// failure's own.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PutSkip {
    pub path: PathBuf,
    #[serde(serialize_with = "error_code")]
    pub code: ErrorCode,
    pub message: String,
}

/// Exit status of a command that finished but skipped some of its inputs, which its
/// result lists in `skipped` (DESIGN §6.3). Not an error and not an `ErrorCode`.
pub const SKIPPED_EXIT: i32 = 9;

/// An error code in a result document as its contract string (DESIGN §6.3).
fn error_code<S: serde::Serializer>(code: &ErrorCode, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(code.as_str())
}

/// The verdict a `review` writes (DESIGN §4.6 `knowa:review`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Accept,
    Reject,
}

impl ReviewDecision {
    fn term(self) -> &'static str {
        match self {
            ReviewDecision::Accept => vocab::REVIEW_ACCEPT,
            ReviewDecision::Reject => vocab::REVIEW_REJECT,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractResult {
    pub blobs: u64,
    pub claims: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunResult {
    pub batches: u64,
    pub claims_in: u64,
    pub claims_out: u64,
    pub cursor: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub path: PathBuf,
    pub level: Visibility,
    pub partial: bool,
    pub claims: u64,
    pub blobs: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub space: Id,
    pub path: PathBuf,
    pub alias: String,
    pub added: u64,
    pub skipped_existing: u64,
    pub blobs_added: u64,
    pub gaps: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceAddResult {
    pub alias: String,
    pub space: Id,
    /// the replica's canonical location; JSON key `path`, as in `InitResult`
    #[serde(rename = "path")]
    pub location: String,
    /// the canonical blob location, when it is not the replica's
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<String>,
    pub readonly: bool,
}

pub struct Knowa {
    home: Home,
}

impl Knowa {
    pub fn new(home: Home) -> Knowa {
        Knowa { home }
    }

    pub fn home(&self) -> &Home {
        &self.home
    }

    // ---------- primitives ----------

    /// DESIGN §4.4 / §6. Under the actor's lock: refuse readonly spaces (`usage`);
    /// mint ids for placeholders; validate drafts (a `knowa:actorKind` draft →
    /// `invalid`: only the system writes it); if this is the actor's first claim in
    /// the space, prepend a `knowa:actorKind` claim; dedupe via equivalence key (same
    /// actor, active and not stale → `created:false`, nothing appended); seal; append; apply
    /// to the index. All-or-nothing per call. On an object store, where another
    /// writer of the actor can append first, the write is redone on the new head, at
    /// most 3 times in all before it fails with `conflict` (DESIGN §5.3).
    pub fn write(&self, sel: &Sel, drafts: Vec<Draft>) -> Result<WriteResult> {
        let space = select::current(&self.home, sel)?;
        let actor = self.home.resolve_actor(sel.actor.as_deref())?;
        Ok(write::Txn::begin(&space, &[&actor])?.write(&actor, drafts)?.result)
    }

    pub fn blob_put(&self, sel: &Sel, r: &mut dyn Read) -> Result<BlobPutResult> {
        let space = select::current(&self.home, sel)?;
        space.ensure_writable()?;
        let (id, created, size) = space.replica.put_blob(r)?;
        // Claims about these bytes may already be indexed (DESIGN §5.3 Catalog-only
        // blobs); storing them writes no claim, so the index must be told. Also when
        // the bytes were already present: an earlier blob put may have crashed between
        // storing them and this refresh. Writing no claim, it takes no actor lock.
        space.index()?.refresh_blob(&space.replica, &id)?;
        Ok(BlobPutResult { id, created, size })
    }

    /// blob.get: the bytes of the blob, from the first selected space that stores it
    /// (`spaces` as in a query doc; empty = current space). Absent everywhere →
    /// `not_found`.
    pub fn blob_open(&self, sel: &Sel, spaces: &[String], id: &Id) -> Result<Box<dyn Read + Send>> {
        if id.kind() != IdKind::Blob {
            return Err(Error::invalid(format!("blob.get: {id} is not a blob id")));
        }
        let base = id.base();
        for space in select::select(&self.home, sel, spaces)? {
            if space.replica.has_blob(&base)? {
                return space.replica.open_blob(&base);
            }
        }
        Err(Error::not_found(format!("blob.get: no selected space stores {base}")))
    }

    /// Validates the doc (limits, reserved `asOf` → `unsupported`, multi-space cursor →
    /// `unsupported`, `nearId` resolution, `order: distance` without near → `invalid`),
    /// fans out to each selected replica's index, and merges:
    /// - find: entities merged by id (spaces unioned, types unioned, display fields from
    ///   the first space in selection order, except `when`, where a `knowa:when` beats a
    ///   file time, which beats a record time, DESIGN §4.8), then ordered by the requested order.
    /// - get / context / path / log as documented in DESIGN §6.2.
    pub fn query(&self, sel: &Sel, doc: QueryDoc) -> Result<QueryResult> {
        query::query(&self.home, sel, doc)
    }

    /// DESIGN §6 feed. `cursor` is one a previous call returned (malformed → `invalid`),
    /// None = from the beginning. It holds across a reindex and on every machine reading
    /// the replica; issued at another gap generation than the replica's now →
    /// `cursor_expired`.
    pub fn feed(&self, sel: &Sel, cursor: Option<&str>, limit: u32) -> Result<FeedResult> {
        feed::feed(&select::current(&self.home, sel)?, cursor, limit)
    }

    // ---------- admin & composite ops ----------

    /// Creates a space at `location` (a new or empty directory, or an empty object
    /// location), its blobs at `blobs` when given (DESIGN §5.2 `--blobs`), writes
    /// genesis (`knowa:owner`), the owner's actorKind, `knowa:label` = name on the space
    /// id, grants the extract actor `knowa:Worker` + `knowa:autoAccept true`, registers
    /// `alias` (default: the location's last name) with both locations and makes it
    /// current if no current space is set.
    pub fn init(
        &self,
        sel: &Sel,
        location: &Location,
        blobs: Option<&Location>,
        name: &str,
        alias: Option<&str>,
    ) -> Result<InitResult> {
        admin::init(&self.home, sel, location, blobs, name, alias)
    }

    /// Ingests files/directories (recursive; hidden files skipped and NOT silently:
    /// they are simply not walked, as documented in CLI help). Per file: blob_put,
    /// `knowa:name` + `knowa:source` (absolute path) + `knowa:modified` + `knowa:created`
    /// (when the OS reports one) by the actor, then (unless `no_extract`) extractors
    /// written as the extract actor. With `no_store` each file is hashed in place
    /// instead of stored, and extractors read the source (DESIGN §5 Catalog-only blobs).
    /// A named path that does not exist → `not_found`, before anything is written. A
    /// file's own failure goes to `skipped` and the run continues (DESIGN §7): it
    /// changed while being stored, or hashed and extracted (`conflict`), vanished
    /// (`not_found`), could not be read, has a non-UTF-8 name or is no longer a regular
    /// file (`invalid`), or is evicted and `download_evicted` is off (`unsupported`).
    /// Failures of the replica, the lock, the index or the claim writes stop the run.
    pub fn put(&self, sel: &Sel, paths: &[PathBuf], opts: PutOptions) -> Result<PutResult> {
        ingest::put(&self.home, sel, paths, opts)
    }

    /// DESIGN §7 `review`: writes one `knowa:review` with `decision` (and `note`) per
    /// target, all in ONE `write`. Targets are `claims` in the given order, then every
    /// **proposed** claim of the current space (the one the write goes to) matching
    /// `select`, oldest first; duplicates keep their first position. Neither claims nor
    /// a selector → `usage`, and so is an empty selector: there is no "review
    /// everything". An id in `claims` that is not a claim id → `invalid`. No target
    /// left → an empty result, nothing written. A review equal to an active one by the
    /// same actor is not written again (`created:false`), by `write`'s equivalence rule.
    pub fn review(&self, sel: &Sel, claims: &[Id], select: Option<Where>, decision: ReviewDecision, note: Option<String>) -> Result<WriteResult> {
        review::review(self, sel, claims, select, decision, note)
    }

    /// Runs built-in extractors on blobs that lack their output (`all` = re-run every
    /// extractor on every blob; idempotent writes make this safe).
    pub fn extract(&self, sel: &Sel, all: bool) -> Result<ExtractResult> {
        ingest::extract(&self.home, sel, all)
    }

    /// DESIGN §7.1. `actor_name` must exist with kind Agent or Worker (else `usage`).
    /// `follow` polls every 2s until interrupted.
    pub fn worker_run(&self, sel: &Sel, actor_name: &str, cmd: &[String], batch: u32, follow: bool) -> Result<WorkerRunResult> {
        worker::worker_run(&self.home, sel, actor_name, cmd, batch, follow)
    }

    pub fn export(&self, sel: &Sel, out: &Path, level: Visibility) -> Result<ExportResult> {
        admin::export(&self.home, sel, out, level)
    }

    /// Imports into `dir` (default `<home>/replicas/<space-id>`), registers it readonly
    /// under `alias` (default: the space's label slugified, else the space id). A
    /// registered replica of the same space id with a different owner or genesis →
    /// `conflict`, before anything is written (DESIGN §7.3).
    pub fn import(&self, bundle: &Path, dir: Option<&Path>, alias: Option<&str>) -> Result<ImportResult> {
        admin::import(&self.home, bundle, dir, alias)
    }

    pub fn verify(&self, sel: &Sel) -> Result<knowa_store::VerifyReport> {
        admin::verify(&self.home, sel)
    }

    /// Rebuilds the current space's index; returns its new epoch. Feed cursors stay
    /// valid: they record per-actor seqs, not positions in this index.
    pub fn reindex(&self, sel: &Sel) -> Result<String> {
        admin::reindex(&self.home, sel)
    }

    /// Registers the existing replica at `location`, its blobs at `blobs` when given,
    /// under `alias` (default: the space's label slugified, else the location's last
    /// name). Local config only; writes no claims. Same identity rule as `import`.
    pub fn space_add(
        &self,
        location: &Location,
        blobs: Option<&Location>,
        alias: Option<&str>,
        readonly: bool,
    ) -> Result<SpaceAddResult> {
        admin::space_add(&self.home, location, blobs, alias, readonly)
    }
}

/// Machine-readable contract for agents (`knowa schema --json`): ops, document shapes
/// (as JSON-Schema-like descriptions), kernel vocabulary, error codes, value syntax.
pub fn schema() -> serde_json::Value {
    schema::schema()
}
