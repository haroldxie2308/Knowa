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
use knowa_model::{Draft, Error, Id, Result, Timestamp, Value, When, vocab};

use crate::select::{Space, current};
use crate::write::{Txn, stamp_worker, term};
use crate::{ExtractResult, PutItem, PutResult, Sel};

pub(crate) fn put(home: &Home, sel: &Sel, paths: &[PathBuf], no_extract: bool) -> Result<PutResult> {
    if paths.is_empty() {
        return Err(Error::invalid("put: no paths given"));
    }
    let space = current(home, sel)?;
    space.ensure_writable()?;
    let actor = home.resolve_actor(sel.actor.as_deref())?;
    let extractor = if no_extract { None } else { Some(home.extract_actor()?) };
    // The whole file list is gathered (and every named path checked) before anything
    // is written.
    let files = collect_files(paths)?;
    let mut items = Vec::with_capacity(files.len());
    for f in &files {
        items.push(put_file(&space, &actor, extractor.as_ref(), f)?);
    }
    Ok(PutResult { items })
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

fn put_file(space: &Space, actor: &Actor, extractor: Option<&Actor>, path: &Path) -> Result<PutItem> {
    let utf8 = |s: Option<&str>, what: &str| {
        s.map(str::to_string)
            .ok_or_else(|| Error::invalid(format!("put: {what} of {} is not valid UTF-8", path.display())))
    };
    let name = utf8(path.file_name().and_then(|n| n.to_str()), "file name")?;
    let source = utf8(path.to_str(), "path")?;
    let disappeared = |e: io::Error| match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("put: file disappeared: {}", path.display())),
        _ => Error::from(e),
    };
    // Times are read before and after the bytes are stored (DESIGN §7, `put`), from the
    // path, so the check covers the file being rewritten or replaced at that path.
    let meta = fs::metadata(path).map_err(disappeared)?;
    let before = Stat::of(&meta)?;
    let mut file = File::open(path).map_err(disappeared)?;
    let (blob, created, size) = space.replica.put_blob(&mut file)?;
    unchanged(path, before, Stat::of(&fs::metadata(path).map_err(disappeared)?)?)?;

    let mut errors = Vec::new();
    let mut drafts = vec![
        Draft::new(blob.clone(), term(vocab::NAME)?, Value::text(name.as_str())),
        Draft::new(blob.clone(), term(vocab::SOURCE)?, Value::text(source)),
    ];
    let birth = match meta.created() {
        Ok(t) => Some(t),
        // Fallback: Linux reports no birth time without statx, or on a filesystem that
        // does not record one. knowa:created is written only when the OS reports one
        // (DESIGN §4.6), so leaving it out is the specified result, not a lost value.
        Err(e) if e.kind() == io::ErrorKind::Unsupported => None,
        Err(e) => return Err(e.into()),
    };
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

    // Extraction is pure and may be slow: it runs before the write lock is taken.
    let extracted = match extractor {
        Some(_) => Some(knowa_extract::run(&blob, &space.replica.blob_path(&blob)?, Some(&name), None)?),
        None => None,
    };

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
    Ok(PutItem { path: path.to_path_buf(), blob, created, size, claims, errors })
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
    use knowa_model::ErrorCode;
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
