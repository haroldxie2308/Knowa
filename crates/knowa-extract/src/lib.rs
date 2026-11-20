//! Built-in, deterministic extractors (DESIGN §7.2). Pure functions from blob bytes
//! to drafts. They never write; the api layer signs their output as the `extract`
//! worker actor with `via.worker = <name>`.

mod decimal;
mod exif_worker;
mod media;

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use knowa_model::{Draft, Error, Id, IdKind, Result};

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

/// Bytes read for content sniffing (the window `infer` itself uses).
const HEAD_BYTES: u64 = 8192;

/// What every extractor sees for one blob.
pub(crate) struct Input<'a> {
    pub(crate) blob: &'a Id,
    pub(crate) path: &'a Path,
    pub(crate) media_type: &'a str,
    pub(crate) size: u64,
}

/// An extractor's verdict on the content: drafts, or why the content could not be
/// read (becomes an `ExtractionError`). Environmental failures (I/O) are the outer
/// `Result` of a worker and abort `run`.
pub(crate) type Outcome = std::result::Result<Vec<Draft>, String>;

struct Worker {
    name: &'static str,
    run: fn(&Input) -> Result<Outcome>,
}

/// The implementations behind `WORKERS`, in the same order.
const REGISTRY: &[Worker] = &[Worker { name: "media@1", run: media::run }, Worker { name: "exif@1", run: exif_worker::run }];

/// Sniffs content (magic bytes via `infer`) first, then the extension of `name_hint`,
/// then `text/plain` if the head is valid UTF-8 without NULs, else
/// `application/octet-stream`.
pub fn sniff_media_type(head: &[u8], name_hint: Option<&str>) -> String {
    media::sniff(head, name_hint)
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
    if blob.kind() != IdKind::Blob {
        return Err(Error::invalid(format!("{blob} is not a blob id")));
    }
    if let Some(names) = only
        && let Some(unknown) = names.iter().find(|n| !REGISTRY.iter().any(|w| w.name == **n))
    {
        return Err(Error::invalid(format!("unknown extractor {unknown:?} (built-in: {WORKERS:?})")));
    }
    let mut file = File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("blob file {} not found", path.display())),
        _ => Error::internal(format!("io: {}: {e}", path.display())),
    })?;
    let io = |e: io::Error| Error::internal(format!("io: {}: {e}", path.display()));
    let size = file.metadata().map_err(io)?.len();
    let mut head = Vec::new();
    (&mut file).take(HEAD_BYTES).read_to_end(&mut head).map_err(io)?;
    let media_type = sniff_media_type(&head, name_hint);
    let input = Input { blob, path, media_type: &media_type, size };

    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for w in REGISTRY.iter().filter(|w| only.is_none_or(|names| names.contains(&w.name))) {
        let drafts = match (w.run)(&input)? {
            Ok(drafts) => drafts,
            Err(message) => {
                failed.push(ExtractionError { worker: w.name, message });
                continue;
            }
        };
        match drafts.iter().find_map(|d| d.validate().err().map(|e| (d, e))) {
            None => done.push(Extraction { worker: w.name, drafts }),
            Some((d, e)) => failed.push(ExtractionError {
                worker: w.name,
                message: format!("{} draft is invalid: {}", d.p, e.message),
            }),
        }
    }
    Ok((done, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_public_worker_list() {
        let names: Vec<&str> = REGISTRY.iter().map(|w| w.name).collect();
        assert_eq!(names, WORKERS);
    }
}
