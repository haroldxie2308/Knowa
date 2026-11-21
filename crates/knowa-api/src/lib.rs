//! The stable core (DESIGN §6): five primitives plus admin ops, composed over
//! store + index + extract + config. The CLI is a thin adapter over this crate; a
//! future MCP server or language binding is another adapter over the same calls.

use std::io::Read;
use std::path::{Path, PathBuf};

use knowa_config::Home;
use knowa_model::query::{BlobPutResult, FeedResult, QueryDoc, QueryResult, WriteResult};
use knowa_model::{Draft, Error, Id, IdKind, Result, Visibility};
use serde::Serialize;

mod admin;
mod context;
mod feed;
mod ingest;
mod merge;
mod path;
mod query;
mod schema;
mod select;
#[cfg(test)]
mod test_graph;
mod worker;
mod write;

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
    pub path: PathBuf,
    pub alias: String,
    pub owner: Id,
    /// the built-in extract worker, granted Worker + autoAccept
    pub extract_actor: Id,
    pub claims: Vec<Id>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PutItem {
    pub path: PathBuf,
    pub blob: Id,
    pub created: bool,
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
    pub path: PathBuf,
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

    /// DESIGN §4.4 / §6. Under the replica lock: refuse readonly spaces (`usage`);
    /// mint ids for placeholders; validate drafts (a `knowa:actorKind` draft →
    /// `invalid`: only the system writes it); if this is the actor's first claim in
    /// the space, prepend a `knowa:actorKind` claim; dedupe via equivalence key (same
    /// actor, active and not stale → `created:false`, nothing appended); seal; append; apply
    /// to the index. All-or-nothing per call.
    pub fn write(&self, sel: &Sel, drafts: Vec<Draft>) -> Result<WriteResult> {
        let space = select::current(&self.home, sel)?;
        let actor = self.home.resolve_actor(sel.actor.as_deref())?;
        Ok(write::Txn::begin(&space)?.write(&actor, drafts)?.result)
    }

    pub fn blob_put(&self, sel: &Sel, r: &mut dyn Read) -> Result<BlobPutResult> {
        let space = select::current(&self.home, sel)?;
        space.ensure_writable()?;
        let (id, created, size) = space.replica.put_blob(r)?;
        Ok(BlobPutResult { id, created, size })
    }

    /// Finds the blob in the selected spaces (`spaces` as in a query doc; empty =
    /// current space). Absent everywhere → `not_found`.
    pub fn blob_path(&self, sel: &Sel, spaces: &[String], id: &Id) -> Result<PathBuf> {
        if id.kind() != IdKind::Blob {
            return Err(Error::invalid(format!("blob.get: {id} is not a blob id")));
        }
        let base = id.base();
        for space in select::select(&self.home, sel, spaces)? {
            if space.replica.has_blob(&base) {
                return space.replica.blob_path(&base);
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

    /// `cursor` = `<epoch>:<n>`; epoch mismatch → `cursor_expired`. None = from start.
    pub fn feed(&self, sel: &Sel, cursor: Option<&str>, limit: u32) -> Result<FeedResult> {
        feed::feed(&select::current(&self.home, sel)?, cursor, limit)
    }

    // ---------- admin & composite ops ----------

    /// Creates a space at `dir`, writes genesis (`knowa:owner`), the owner's actorKind,
    /// `knowa:label` = name on the space id, grants the extract actor `knowa:Worker` +
    /// `knowa:autoAccept true`, registers `alias` (default: dir basename) and makes it
    /// current if no current space is set.
    pub fn init(&self, sel: &Sel, dir: &Path, name: &str, alias: Option<&str>) -> Result<InitResult> {
        admin::init(&self.home, sel, dir, name, alias)
    }

    /// Ingests files/directories (recursive; hidden files skipped and NOT silently:
    /// they are simply not walked, as documented in CLI help). Per file: blob_put,
    /// `knowa:name` + `knowa:source` (absolute path) + `knowa:modified` + `knowa:created`
    /// (when the OS reports one) by the actor, then (unless `no_extract`) extractors
    /// written as the extract actor. Nonexistent path → `not_found`; a file whose size or
    /// modification time changed while being stored → `conflict` (DESIGN §7).
    pub fn put(&self, sel: &Sel, paths: &[PathBuf], no_extract: bool) -> Result<PutResult> {
        ingest::put(&self.home, sel, paths, no_extract)
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

    /// Rebuilds the current space's index; returns the new epoch (feed cursors from the
    /// old epoch now fail with `cursor_expired`).
    pub fn reindex(&self, sel: &Sel) -> Result<String> {
        admin::reindex(&self.home, sel)
    }

    /// Registers an existing replica directory under `alias` (default: the space's label
    /// slugified, else the directory name). Local config only; writes no claims. Same
    /// identity rule as `import`.
    pub fn space_add(&self, dir: &Path, alias: Option<&str>, readonly: bool) -> Result<SpaceAddResult> {
        admin::space_add(&self.home, dir, alias, readonly)
    }
}

/// Machine-readable contract for agents (`knowa schema --json`): ops, document shapes
/// (as JSON-Schema-like descriptions), kernel vocabulary, error codes, value syntax.
pub fn schema() -> serde_json::Value {
    schema::schema()
}
