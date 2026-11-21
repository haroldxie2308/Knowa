//! `put` (ingest files) and `extract` (built-in extractors over stored blobs),
//! DESIGN §7, §7.2.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use knowa_config::{Actor, Home};
use knowa_extract::{Extraction, ExtractionError, WORKERS};
use knowa_model::query::{Order, View, Where};
use knowa_model::{Draft, Error, ErrorCode, Id, Result, Timestamp, Value, When, vocab};

use crate::select::{Space, current};
use crate::write::{Txn, stamp_worker, term};
use crate::{ExtractResult, PutItem, PutOptions, PutResult, PutSkip, Sel};

pub(crate) fn put(home: &Home, sel: &Sel, paths: &[PathBuf], opts: PutOptions) -> Result<PutResult> {
    if paths.is_empty() {
        return Err(Error::invalid("put: no paths given"));
    }
    let space = current(home, sel)?;
    space.ensure_writable()?;
    let actor = home.resolve_actor(sel.actor.as_deref())?;
    let extractor = if opts.no_extract { None } else { Some(home.extract_actor()?) };
    // The whole file list is gathered (and every named path checked) before anything
    // is written.
    let files = collect_files(paths)?;
    let (mut items, mut skipped) = (Vec::with_capacity(files.len()), Vec::new());
    for f in &files {
        match put_file(&space, &actor, extractor.as_ref(), opts, f) {
            Ok(item) => items.push(item),
            // Source errors carry no `detail`; code and message are the whole failure.
            Err(Failure::File(e)) => skipped.push(PutSkip { path: f.clone(), code: e.code, message: e.message }),
            Err(Failure::Run(e)) => return Err(e),
        }
    }
    Ok(PutResult { items, skipped })
}

/// Where a failure during `put` arose, which decides what it stops (DESIGN §7, `put`).
enum Failure {
    /// From the source file alone: it changed while being added, vanished, could not be
    /// read, is not a regular file, has a non-UTF-8 name, or is evicted. Skips the file.
    File(Error),
    /// From the replica, the lock, the index or the claim writes. Stops the run.
    Run(Error),
}

/// Unmarked errors stop the run: only the steps that read the source alone mark their
/// errors `Failure::File`, so a failure nobody classified never passes as a file's own.
impl From<Error> for Failure {
    fn from(e: Error) -> Failure {
        Failure::Run(e)
    }
}

/// Explicitly named paths are followed even if they are symlinks; directories are
/// walked recursively without following symlinks and without entering or listing
/// names that start with `.`. Absolute paths, deduplicated, in walk order.
pub(crate) fn collect_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        let meta = fs::metadata(p).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::not_found(format!("put: no such file or directory: {}", p.display())),
            _ => Error::from(e),
        })?;
        let abs = std::path::absolute(p)?;
        if meta.is_dir() {
            walk(&abs, &mut out)?;
        } else if meta.is_file() {
            out.push(abs);
        } else {
            return Err(Error::invalid(format!("put: not a regular file or directory: {}", p.display())));
        }
    }
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if e.file_name().as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        // `DirEntry::file_type` does not follow symlinks: links, sockets, FIFOs and
        // devices met during a walk are not ingested.
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk(&e.path(), out)?;
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
    Ok(())
}

/// Extractor successes and per-extractor failures for one file.
type Extracted = (Vec<Extraction>, Vec<ExtractionError>);

/// One file's `put`. Every step that reads the source alone (its name, stat, evicted
/// check and download, the readability check, hashing it and extracting from it in
/// place, the change check) marks its errors `Failure::File`; everything else stops
/// the run (see `Failure`).
fn put_file(
    space: &Space,
    actor: &Actor,
    extractor: Option<&Actor>,
    opts: PutOptions,
    path: &Path,
) -> std::result::Result<PutItem, Failure> {
    let utf8 = |s: Option<&str>, what: &str| {
        s.map(str::to_string)
            .ok_or_else(|| Failure::File(Error::invalid(format!("put: {what} of {} is not valid UTF-8", path.display()))))
    };
    let name = utf8(path.file_name().and_then(|n| n.to_str()), "file name")?;
    let source = utf8(path.to_str(), "path")?;
    let stat = || fs::metadata(path).map_err(|e| source_err(path, e));
    // Extraction is pure and may be slow: it runs before the write lock is taken.
    let extract = |blob: &Id, bytes: &Path| -> Result<Option<Extracted>> {
        extractor.map(|_| knowa_extract::run(blob, bytes, Some(&name), None)).transpose()
    };
    // Times are read before and after the bytes are obtained (DESIGN §7, `put`), from
    // the path, so the check covers the file being rewritten or replaced at that path.
    // `fs::metadata` does not download an evicted file.
    let mut meta = stat().map_err(Failure::File)?;
    if evicted(&meta) {
        if !opts.download_evicted {
            return Err(Failure::File(Error::unsupported(format!(
                "put: {} is evicted to the cloud and was not downloaded; use --download-evicted to download and add it",
                path.display()
            ))));
        }
        // Scenario: an evicted file (e.g. iCloud "Optimize Mac Storage") the caller asked
        // for. Reading it makes the OS download it and rewrite its metadata, so it is read
        // once in full first and its times are taken afterwards: they and the change
        // check then describe the downloaded file, not the placeholder.
        download(path).map_err(Failure::File)?;
        meta = stat().map_err(Failure::File)?;
    }
    // Checked before anything opens the file: opening a FIFO would block.
    if !meta.is_file() {
        return Err(Failure::File(Error::invalid(format!("put: not a regular file: {}", path.display()))));
    }
    let before = Stat::of(&meta).map_err(Failure::File)?;
    let birth = match meta.created() {
        Ok(t) => Some(t),
        // Fallback: Linux reports no birth time without statx, or on a filesystem that
        // does not record one. knowa:created is written only when the OS reports one
        // (DESIGN §4.6), so leaving it out is the specified result, not a lost value.
        Err(e) if e.kind() == io::ErrorKind::Unsupported => None,
        Err(e) => return Err(Failure::File(source_err(path, e))),
    };
    let after = || unchanged(path, before, Stat::of(&stat()?)?);
    let (blob, created, stored, size, extracted) = if opts.no_store {
        // Catalog only (DESIGN §5 Catalog-only blobs): hashed in place, never copied.
        // Every step reads only the source, so every error here is the file's own.
        let (blob, size) = knowa_store::hash_file(path).map_err(Failure::File)?;
        // Extractors read the source, which can still change: the check runs after
        // them, so their output belongs to the hashed bytes as much as the times do.
        // Besides reading the file, `run` fails only on a non-blob id or an unknown
        // worker name, and `put` passes neither, so its errors here are the source's.
        let extracted = extract(&blob, path).map_err(Failure::File)?;
        after().map_err(Failure::File)?;
        let stored = space.replica.has_blob(&blob);
        (blob, false, stored, size, extracted)
    } else {
        // The store reports a source it cannot open as `internal`, the code its own I/O
        // failures in `blobs/` carry too. Opening the source here first tells them
        // apart: one that cannot be opened is the file's failure. One that turns
        // unreadable between this and the copy fails the copy and stops the run.
        File::open(path).map_err(|e| Failure::File(source_err(path, e)))?;
        // Same-volume APFS sources are stored as a clone (DESIGN §5 Blob path).
        let (blob, created, size) = space.replica.put_blob_file(path).map_err(copy_failure)?;
        after().map_err(Failure::File)?;
        // Extractors read the stored blob, which is immutable.
        let extracted = extract(&blob, &space.replica.blob_path(&blob)?)?;
        (blob, created, true, size, extracted)
    };

    let mut errors = Vec::new();
    let mut drafts = vec![
        Draft::new(blob.clone(), term(vocab::NAME)?, Value::text(name.as_str())),
        Draft::new(blob.clone(), term(vocab::SOURCE)?, Value::text(source)),
    ];
    for (p, t) in [(vocab::MODIFIED, Some(before.modified)), (vocab::CREATED, birth)] {
        let Some(t) = t else { continue };
        match whole_seconds(t)? {
            Some(w) => drafts.push(Draft::new(blob.clone(), term(p)?, Value::When(w))),
            // Degraded but correct: a time outside the EDTF subset (years 0000–9999) has
            // no claim form. The file is still stored and named; the claim is left out and
            // reported, never replaced by an invented time. Without knowa:modified the
            // entity's when falls back to record time (DESIGN §4.8), which is labelled.
            None => errors.push(format!(
                "{p} not written for {}: its time, {}, is outside the EDTF range (years 0000-9999)",
                path.display(),
                since_epoch(t)
            )),
        }
    }

    let mut txn = Txn::begin(space)?;
    let described = txn.write(actor, drafts)?;
    let mut claims: Vec<Id> = described.result.items.into_iter().map(|i| i.id).collect();
    if let (Some(x), Some((extractions, failures))) = (extractor, extracted) {
        let drafts = stamped(extractions)?;
        if !drafts.is_empty() {
            claims.extend(txn.write(x, drafts)?.result.items.into_iter().map(|i| i.id));
        }
        errors.extend(failures.iter().map(describe_failure));
    }
    if stored {
        // Every claim about the blob is applied by now; when they were all already
        // there (it was put with --no-store before), nothing above indexed its text.
        // Not only when `created`: a crash after the bytes were stored and before this
        // refresh committed would otherwise leave the text unindexed until reindex,
        // since the next put finds the bytes present. Any later put repairs it.
        txn.refresh_blob(&blob)?;
    }
    Ok(PutItem { path: path.to_path_buf(), blob, created, stored, size, claims, errors })
}

/// An I/O failure on the source file itself.
fn source_err(path: &Path, e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("put: file disappeared: {}", path.display())),
        _ => Error::internal(format!("io: {}: {e}", path.display())),
    }
}

/// `put_blob_file` reads the source and writes the replica, and reports an I/O failure
/// of either, mid-copy included, as `internal`: those stop the run. Only the codes it
/// gives the source alone are the file's own: `not_found` (vanished) and `invalid`
/// (no longer a regular file) come from its check of the source and nowhere else.
fn copy_failure(e: Error) -> Failure {
    match e.code {
        ErrorCode::NotFound | ErrorCode::Invalid => Failure::File(e),
        _ => Failure::Run(e),
    }
}

/// Reads `path` to the end and discards the bytes, which makes the OS download an
/// evicted file.
fn download(path: &Path) -> Result<()> {
    let mut f = File::open(path).map_err(|e| source_err(path, e))?;
    io::copy(&mut f, &mut io::sink()).map_err(|e| source_err(path, e))?;
    Ok(())
}

/// True if `meta` describes an evicted file: its contents are in the cloud and reading
/// it downloads them (DESIGN §7, `put`).
#[cfg(target_os = "macos")]
fn evicted(meta: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    dataless(meta.st_flags())
}

/// Elsewhere nothing is evicted: dataless files are a macOS feature.
#[cfg(not(target_os = "macos"))]
fn evicted(_meta: &fs::Metadata) -> bool {
    false
}

/// True if BSD file flags `st_flags` mark a dataless file. Value from macOS
/// `<sys/stat.h>` (std has the field but not the constant).
#[cfg(target_os = "macos")]
fn dataless(st_flags: u32) -> bool {
    const SF_DATALESS: u32 = 0x4000_0000;
    st_flags & SF_DATALESS != 0
}

/// What must not change while `put` stores a file (DESIGN §7, `put`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    modified: SystemTime,
}

impl Stat {
    fn of(meta: &fs::Metadata) -> Result<Stat> {
        Ok(Stat { len: meta.len(), modified: meta.modified()? })
    }
}

/// A file whose size or modification time changed while its bytes were stored is a
/// `conflict`: the stored bytes and the times read would not belong together.
fn unchanged(path: &Path, before: Stat, after: Stat) -> Result<()> {
    if before == after {
        return Ok(());
    }
    Err(Error::conflict(format!(
        "put: {} changed while being added; retry once it stops changing",
        path.display()
    )))
}

/// `t` as the EDTF `YYYY-MM-DDThh:mm:ssZ`: whole seconds UTC, floored toward negative
/// infinity so pre-1970 times land on the second they fall in (DESIGN §4.6). `None` if
/// the time has no form in the EDTF subset (DESIGN §4.5).
fn whole_seconds(t: SystemTime) -> Result<Option<When>> {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(e) => {
            let before = e.duration();
            i64::try_from(before.as_secs()).ok().map(|s| -s - i64::from(before.subsec_nanos() != 0))
        }
    };
    let Some(ms) = secs.and_then(|s| s.checked_mul(1000)) else { return Ok(None) };
    let stamp = Timestamp(ms).format();
    // `format` ends in milliseconds, which are zero for whole seconds.
    let Some(head) = stamp.strip_suffix(".000Z") else {
        return Err(Error::internal(format!("whole-second timestamp formatted as {stamp:?}")));
    };
    let edtf = format!("{head}Z");
    match When::parse(&edtf) {
        Ok(w) => Ok(Some(w)),
        // Outside years 0000–9999 `format` writes an expanded year (`+10000-…`,
        // `-0001-…`), which the subset has no form for.
        Err(_) if edtf.starts_with(['+', '-']) => Ok(None),
        Err(e) => Err(Error::internal(format!("file time {edtf:?} is not valid EDTF: {}", e.message))),
    }
}

/// A file time as an offset from the Unix epoch, for messages about times that have
/// no EDTF form.
fn since_epoch(t: SystemTime) -> String {
    match t.duration_since(UNIX_EPOCH) {
        Ok(after) => format!("{after:?} after the Unix epoch"),
        Err(e) => format!("{:?} before the Unix epoch", e.duration()),
    }
}

/// Extractor drafts carry `via.worker = <extractor name>` (DESIGN §7.2).
fn stamped(extractions: Vec<Extraction>) -> Result<Vec<Draft>> {
    let mut out = Vec::new();
    for x in extractions {
        for mut d in x.drafts {
            stamp_worker(&mut d, x.worker).map_err(|e| Error::internal(format!("extractor {}: {}", x.worker, e.message)))?;
            out.push(d);
        }
    }
    Ok(out)
}

fn describe_failure(f: &ExtractionError) -> String {
    format!("{}: {}", f.worker, f.message)
}

pub(crate) fn extract(home: &Home, sel: &Sel, all: bool) -> Result<ExtractResult> {
    let space = current(home, sel)?;
    space.ensure_writable()?;
    let extractor = home.extract_actor()?;
    let index = space.index()?;

    // blob → extractors to run on it, in WORKERS order
    let mut todo: BTreeMap<Id, Vec<&'static str>> = BTreeMap::new();
    if all {
        for b in space.replica.blob_ids()? {
            todo.insert(b, WORKERS.to_vec());
        }
    } else {
        for &w in WORKERS {
            for b in index.blobs_without_worker(w)? {
                todo.entry(b).or_default().push(w);
            }
        }
    }

    let name_pred = term(vocab::NAME)?;
    let mut result = ExtractResult { blobs: todo.len() as u64, claims: 0, errors: Vec::new() };
    for (blob, workers) in todo {
        let hint = name_hint(&index, &blob, &name_pred)?;
        let path = space.replica.blob_path(&blob)?;
        let (extractions, failures) = knowa_extract::run(&blob, &path, hint.as_deref(), Some(&workers))?;
        result.errors.extend(failures.iter().map(|f| format!("{blob}: {}", describe_failure(f))));
        let drafts = stamped(extractions)?;
        if drafts.is_empty() {
            continue;
        }
        let mut txn = Txn::begin(&space)?;
        result.claims += txn.write(&extractor, drafts)?.created;
    }
    Ok(result)
}

/// The blob's most recent `knowa:name`: media sniffing falls back to the file
/// extension, and `put` passed the name too, so re-extraction sniffs the same way.
fn name_hint(index: &knowa_index::Index, blob: &Id, name_pred: &knowa_model::Term) -> Result<Option<String>> {
    let w = Where { s: vec![blob.clone()], p: vec![name_pred.clone()], ..Where::default() };
    let page = index.find_claims(&w, &View::default(), Order::RecordedDesc, 1, None)?;
    Ok(page.items.into_iter().next().and_then(|cv| match cv.claim.body.o {
        Value::Text { text, .. } => Some(text),
        // A knowa:name with a non-text value is a valid claim but names no file:
        // sniff from content alone, exactly as for a blob that has no name.
        _ => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("knowa-api-ingest-{name}-{}", std::process::id()));
        // Leftover from an aborted earlier run, if any; absence is the normal case.
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    #[test]
    fn walk_skips_hidden_and_symlinks_and_sorts() {
        let d = tmp("walk");
        touch(&d.join("b.txt"));
        touch(&d.join("a/c.txt"));
        touch(&d.join(".hidden"));
        touch(&d.join(".git/config"));
        touch(&d.join("outside/target.txt"));
        symlink(d.join("outside/target.txt"), d.join("a/link.txt")).unwrap();
        symlink(d.join("outside"), d.join("dirlink")).unwrap();
        let files = collect_files(std::slice::from_ref(&d)).unwrap();
        assert_eq!(files, vec![d.join("a/c.txt"), d.join("b.txt"), d.join("outside/target.txt")]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn named_symlink_and_hidden_files_are_followed() {
        let d = tmp("named");
        touch(&d.join("target.txt"));
        touch(&d.join(".dotfile"));
        symlink(d.join("target.txt"), d.join("link.txt")).unwrap();
        let files = collect_files(&[d.join("link.txt"), d.join(".dotfile"), d.join("link.txt")]).unwrap();
        assert_eq!(files, vec![d.join("link.txt"), d.join(".dotfile")]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_size_or_mtime_change_while_storing_is_a_conflict() {
        let path = Path::new("/photos/a.jpg");
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_400_000_000);
        let before = Stat { len: 10, modified: t };
        assert!(unchanged(path, before, before).is_ok());
        for after in [Stat { len: 11, ..before }, Stat { modified: t + std::time::Duration::from_nanos(1), ..before }] {
            let err = unchanged(path, before, after).unwrap_err();
            assert_eq!(err.code, ErrorCode::Conflict);
            assert!(err.message.contains("/photos/a.jpg"), "{}", err.message);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_dataless_flag_marks_a_file_evicted() {
        const SF_DATALESS: u32 = 0x4000_0000;
        // UF_IMMUTABLE (Finder "Locked"), UF_HIDDEN, SF_RESTRICTED (SIP)
        let others = 0x0000_0002 | 0x0000_8000 | 0x0008_0000;
        assert!(!dataless(0));
        assert!(!dataless(others));
        assert!(dataless(SF_DATALESS));
        assert!(dataless(SF_DATALESS | others));
    }

    #[test]
    fn only_source_codes_from_the_copy_skip_the_file() {
        let is_file = |code| matches!(copy_failure(Error::new(code, "x")), Failure::File(_));
        assert!(is_file(ErrorCode::NotFound));
        assert!(is_file(ErrorCode::Invalid));
        // `internal` is any I/O failure, the replica's included; the rest are never the source's.
        for code in [ErrorCode::Internal, ErrorCode::Usage, ErrorCode::Conflict, ErrorCode::Unsupported, ErrorCode::Integrity] {
            assert!(!is_file(code), "{code:?}");
        }
    }

    #[test]
    fn file_times_are_whole_utc_seconds_floored() {
        use std::time::Duration;
        let edtf = |t: SystemTime| whole_seconds(t).unwrap().map(|w| w.raw);
        let at = |secs: u64, nanos: u32| UNIX_EPOCH + Duration::new(secs, nanos);
        let before = |secs: u64, nanos: u32| UNIX_EPOCH - Duration::new(secs, nanos);
        assert_eq!(edtf(at(1_400_000_000, 999_999_999)).as_deref(), Some("2014-05-13T16:53:20Z"));
        assert_eq!(edtf(UNIX_EPOCH).as_deref(), Some("1970-01-01T00:00:00Z"));
        // Pre-1970: a partial second belongs to the second below it.
        assert_eq!(edtf(before(0, 1)).as_deref(), Some("1969-12-31T23:59:59Z"));
        assert_eq!(edtf(before(1, 0)).as_deref(), Some("1969-12-31T23:59:59Z"));
        assert_eq!(edtf(before(1, 500_000_000)).as_deref(), Some("1969-12-31T23:59:58Z"));
        // The EDTF subset's ends: 0000-01-01 and 9999-12-31T23:59:59.
        assert_eq!(edtf(before(62_167_219_200, 0)).as_deref(), Some("0000-01-01T00:00:00Z"));
        assert_eq!(edtf(at(253_402_300_799, 0)).as_deref(), Some("9999-12-31T23:59:59Z"));
        assert_eq!(edtf(before(62_167_219_200, 1)), None);
        assert_eq!(edtf(at(253_402_300_800, 0)), None);
    }

    #[test]
    fn missing_path_is_not_found() {
        let d = tmp("missing");
        let err = collect_files(&[d.join("nope")]).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        fs::remove_dir_all(&d).unwrap();
    }
}
