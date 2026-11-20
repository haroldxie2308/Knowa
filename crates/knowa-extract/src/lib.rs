//! Built-in, deterministic extractors (DESIGN §7.2). Pure functions from blob bytes
//! to drafts. They never write; the api layer signs their output as the `extract`
//! worker actor with `via.worker = <name>`.

use std::path::Path;

use knowa_model::{Draft, Id, Result};

/// One extractor's output for one blob.
#[derive(Debug, Clone, PartialEq)]
pub struct Extraction {
    /// "media@1", "exif@1", …
    pub worker: &'static str,
    pub drafts: Vec<Draft>,
}

/// Per-extractor failure on one blob (e.g. corrupt EXIF). Reported to the user by the
/// api layer; does not abort other extractors or other files.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractionError {
    pub worker: &'static str,
    pub message: String,
}

/// Names of all built-in extractors in run order. `media@1` first: later extractors
/// depend on its media type.
pub const WORKERS: &[&str] = &["media@1", "exif@1"];

/// Sniffs content (magic bytes via `infer`) first, then the extension of `name_hint`,
/// then `text/plain` if the head is valid UTF-8 without NULs, else
/// `application/octet-stream`.
pub fn sniff_media_type(head: &[u8], name_hint: Option<&str>) -> String {
    let _ = (head, name_hint);
    todo!()
}

/// Runs every extractor in WORKERS order (or only those in `only`, still in order) on
/// the blob stored at `path` whose id is `blob`. Subjects of all drafts are `blob`.
///
/// - media@1 → `knowa:mediaType` (text), `knowa:size` (int), `knowa:type`
///   (`knowa:Photo` for image/*, else `knowa:File`).
/// - exif@1 (only for image/jpeg, image/heic, image/heif, image/png, image/tiff,
///   image/webp) → `knowa:when` from DateTimeOriginal (+OffsetTimeOriginal; without an
///   offset the EDTF has no offset → local-unknown), `knowa:where` from GPS
///   (lat/lon as decimal strings with ≤ 7 fraction digits, alt if present),
///   `exif:make`, `exif:model` (text), `exif:width`, `exif:height`, `exif:orientation` (int).
///   No EXIF present → Ok with zero drafts (not an error).
///
/// Returns successes and per-extractor errors separately.
pub fn run(blob: &Id, path: &Path, name_hint: Option<&str>, only: Option<&[&str]>) -> Result<(Vec<Extraction>, Vec<ExtractionError>)> {
    let _ = (blob, path, name_hint, only);
    todo!()
}
