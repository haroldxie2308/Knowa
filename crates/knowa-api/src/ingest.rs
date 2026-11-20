//! `put` (ingest files) and `extract` (built-in extractors over stored blobs),
//! DESIGN §7, §7.2.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use knowa_config::{Actor, Home};
use knowa_extract::{Extraction, ExtractionError, WORKERS};
use knowa_model::query::{Order, View, Where};
use knowa_model::{Draft, Error, Id, Result, Value, vocab};

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
    let mut file = File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("put: file disappeared: {}", path.display())),
        _ => Error::from(e),
    })?;
    let (blob, created, size) = space.replica.put_blob(&mut file)?;

    // Extraction is pure and may be slow: it runs before the write lock is taken.
    let extracted = match extractor {
        Some(_) => Some(knowa_extract::run(&blob, &space.replica.blob_path(&blob)?, Some(&name), None)?),
        None => None,
    };

    let mut txn = Txn::begin(space)?;
    let described = txn.write(
        actor,
        vec![
            Draft::new(blob.clone(), term(vocab::NAME)?, Value::text(name)),
            Draft::new(blob.clone(), term(vocab::SOURCE)?, Value::text(source)),
        ],
    )?;
    let mut claims: Vec<Id> = described.result.items.into_iter().map(|i| i.id).collect();
    let mut errors = Vec::new();
    if let (Some(x), Some((extractions, failures))) = (extractor, extracted) {
        let drafts = stamped(extractions)?;
        if !drafts.is_empty() {
            claims.extend(txn.write(x, drafts)?.result.items.into_iter().map(|i| i.id));
        }
        errors = failures.iter().map(describe_failure).collect();
    }
    Ok(PutItem { path: path.to_path_buf(), blob, created, size, claims, errors })
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
    fn missing_path_is_not_found() {
        let d = tmp("missing");
        let err = collect_files(&[d.join("nope")]).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        fs::remove_dir_all(&d).unwrap();
    }
}
