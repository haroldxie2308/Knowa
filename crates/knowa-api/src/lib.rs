//! The stable core (DESIGN §6): five primitives plus admin ops, composed over
//! store + index + extract + config. The CLI is a thin adapter over this crate; a
//! future MCP server or language binding is another adapter over the same calls.

use std::io::Read;
use std::path::{Path, PathBuf};

use knowa_config::Home;
use knowa_model::query::{BlobPutResult, FeedResult, QueryDoc, QueryResult, WriteResult};
use knowa_model::{Draft, Id, Result, Visibility};
use serde::Serialize;

/// Space/actor selectors as given by the caller (flag or env), unresolved.
#[derive(Debug, Clone, Default)]
pub struct Sel {
    pub space: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
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
pub struct PutItem {
    pub path: PathBuf,
    pub blob: Id,
    pub created: bool,
    pub size: u64,
    /// ids of claims written for this file (by the actor and by the extract worker)
    pub claims: Vec<Id>,
    /// "<worker>: <message>" for extractor failures on this file
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PutResult {
    pub items: Vec<PutItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractResult {
    pub blobs: u64,
    pub claims: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerRunResult {
    pub batches: u64,
    pub claims_in: u64,
    pub claims_out: u64,
    pub cursor: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub path: PathBuf,
    pub level: Visibility,
    pub partial: bool,
    pub claims: u64,
    pub blobs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    pub space: Id,
    pub path: PathBuf,
    pub alias: String,
    pub added: u64,
    pub skipped_existing: u64,
    pub blobs_added: u64,
    pub gaps: u64,
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
    /// mint ids for placeholders; validate drafts; if this is the actor's first claim in
    /// the space, prepend a `knowa:actorKind` claim; dedupe via equivalence key (same
    /// actor, not retracted → `created:false`, nothing appended); seal; append; apply
    /// to the index. All-or-nothing per call.
    pub fn write(&self, sel: &Sel, drafts: Vec<Draft>) -> Result<WriteResult> {
        let _ = (sel, drafts);
        todo!()
    }

    pub fn blob_put(&self, sel: &Sel, r: &mut dyn Read) -> Result<BlobPutResult> {
        let _ = (sel, r);
        todo!()
    }

    /// Finds the blob in the selected spaces (`spaces` as in a query doc; empty =
    /// current space). Absent everywhere → `not_found`.
    pub fn blob_path(&self, sel: &Sel, spaces: &[String], id: &Id) -> Result<PathBuf> {
        let _ = (sel, spaces, id);
        todo!()
    }

    /// Validates the doc (limits, reserved `asOf` → `unsupported`, multi-space cursor →
    /// `unsupported`, `nearId` resolution, `order: distance` without near → `invalid`),
    /// fans out to each selected replica's index, and merges:
    /// - find: entities merged by id (spaces unioned, types unioned, display fields from
    ///   the first space in selection order), then ordered by the requested order.
    /// - get / context / path / log as documented in DESIGN §6.2.
    pub fn query(&self, sel: &Sel, doc: QueryDoc) -> Result<QueryResult> {
        let _ = (sel, doc);
        todo!()
    }

    /// `cursor` = `<epoch>:<n>`; epoch mismatch → `cursor_expired`. None = from start.
    pub fn feed(&self, sel: &Sel, cursor: Option<&str>, limit: u32) -> Result<FeedResult> {
        let _ = (sel, cursor, limit);
        todo!()
    }

    // ---------- admin & composite ops ----------

    /// Creates a space at `dir`, writes genesis (`knowa:owner`), the owner's actorKind,
    /// `knowa:label` = name on the space id, grants the extract actor `knowa:Worker` +
    /// `knowa:autoAccept true`, registers `alias` (default: dir basename) and makes it
    /// current if no current space is set.
    pub fn init(&self, sel: &Sel, dir: &Path, name: &str, alias: Option<&str>) -> Result<InitResult> {
        let _ = (sel, dir, name, alias);
        todo!()
    }

    /// Ingests files/directories (recursive; hidden files skipped and NOT silently:
    /// they are simply not walked, as documented in CLI help). Per file: blob_put,
    /// `knowa:name` + `knowa:source` (absolute path) by the actor, then (unless
    /// `no_extract`) extractors written as the extract actor. Nonexistent path → `not_found`.
    pub fn put(&self, sel: &Sel, paths: &[PathBuf], no_extract: bool) -> Result<PutResult> {
        let _ = (sel, paths, no_extract);
        todo!()
    }

    /// Runs built-in extractors on blobs that lack their output (`all` = re-run every
    /// extractor on every blob; idempotent writes make this safe).
    pub fn extract(&self, sel: &Sel, all: bool) -> Result<ExtractResult> {
        let _ = (sel, all);
        todo!()
    }

    /// DESIGN §7.1. `actor_name` must exist with kind Agent or Worker (else `usage`).
    /// `follow` polls every 2s until interrupted.
    pub fn worker_run(&self, sel: &Sel, actor_name: &str, cmd: &[String], batch: u32, follow: bool) -> Result<WorkerRunResult> {
        let _ = (sel, actor_name, cmd, batch, follow);
        todo!()
    }

    pub fn export(&self, sel: &Sel, out: &Path, level: Visibility) -> Result<ExportResult> {
        let _ = (sel, out, level);
        todo!()
    }

    /// Imports into `dir` (default `<home>/replicas/<space-id>`), registers it readonly
    /// under `alias` (default: the space's label slugified, else the space id).
    pub fn import(&self, bundle: &Path, dir: Option<&Path>, alias: Option<&str>) -> Result<ImportResult> {
        let _ = (bundle, dir, alias);
        todo!()
    }

    pub fn verify(&self, sel: &Sel) -> Result<knowa_store::VerifyReport> {
        let _ = sel;
        todo!()
    }

    pub fn reindex(&self, sel: &Sel) -> Result<String> {
        let _ = sel;
        todo!()
    }
}

/// Machine-readable contract for agents (`knowa schema --json`): ops, document shapes
/// (as JSON-Schema-like descriptions), kernel vocabulary, error codes, value syntax.
pub fn schema() -> serde_json::Value {
    todo!()
}
