//! Bundles (DESIGN §7.3): a tar of `bundle.json`, then `log/<actor>/<seq>.jsonl`
//! segments, then `blobs/<fanout>/<id>`. Entries use the same relative paths as a
//! replica, fixed metadata (mode 0644, mtime 0, uid/gid 0) and sorted order, so the
//! same inputs always produce the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use knowa_model::vocab::FORMAT_VERSION;
use knowa_model::{Claim, Error, ErrorCode, Id, IdKind, Result, Timestamp};

use crate::fsutil::{TempFile, io_err};
use crate::log::{self, Chain, SEGMENT_LIMIT};
use crate::{BundleMeta, ImportReport, MANIFEST_FILE, Manifest, Replica, blob, genesis_problem, mark_rebuild};

const META_ENTRY: &str = "bundle.json";
const LEVELS: &[&str] = &["private", "shared", "public"];

fn validate_meta(meta: &BundleMeta) -> Result<()> {
    if meta.format != FORMAT_VERSION {
        return Err(Error::unsupported(format!(
            "bundle format {} (this build reads format {FORMAT_VERSION})",
            meta.format
        )));
    }
    if !LEVELS.contains(&meta.level.as_str()) {
        return Err(Error::invalid(format!("bundle level {:?} is not one of {LEVELS:?}", meta.level)));
    }
    // DESIGN §7.3: only a private export carries everything.
    let partial = meta.level != "private";
    if meta.partial != partial {
        return Err(Error::invalid(format!("a {} bundle must have partial={partial}", meta.level)));
    }
    Timestamp::parse(&meta.at).map_err(|e| Error::invalid(format!("bundle at: {}", e.message)))?;
    for (field, id, kind) in [
        ("space", &meta.space, IdKind::Space),
        ("owner", &meta.owner, IdKind::Actor),
        ("genesis", &meta.genesis, IdKind::Claim),
        ("exportedBy", &meta.exported_by, IdKind::Actor),
    ] {
        if id.kind() != kind {
            return Err(Error::invalid(format!("bundle {field} {id} is not a {kind:?} id")));
        }
    }
    Ok(())
}

fn manifest_of(meta: &BundleMeta) -> Manifest {
    Manifest { format: meta.format, space: meta.space.clone(), owner: meta.owner.clone(), genesis: meta.genesis.clone() }
}

fn append_entry<W: Write>(tar: &mut tar::Builder<W>, path: &str, size: u64, data: impl Read) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    tar.append_data(&mut header, path, data)
        .map_err(|e| Error::internal(format!("bundle entry {path}: {e}")))
}

pub(crate) fn write(replica: &Replica, out: &Path, meta: &BundleMeta, claims: &[Claim], blobs: &[Id]) -> Result<()> {
    validate_meta(meta)?;
    if manifest_of(meta) != *replica.manifest() {
        return Err(Error::invalid("bundle meta does not describe this replica (format/space/owner/genesis differ)"));
    }
    let mut by_actor: BTreeMap<&Id, BTreeMap<u64, &Claim>> = BTreeMap::new();
    for c in claims {
        if c.body.space != meta.space {
            return Err(Error::invalid(format!("claim {} belongs to space {}, not {}", c.id, c.body.space, meta.space)));
        }
        if by_actor.entry(&c.body.actor).or_default().insert(c.body.seq, c).is_some() {
            return Err(Error::invalid(format!("seq {} of {} is given twice", c.body.seq, c.body.actor)));
        }
    }
    let mut blob_files = BTreeMap::new();
    for b in blobs {
        blob_files.insert(b.base(), replica.blob_path(b)?);
    }

    let dir = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = TempFile::create_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let mut tar = tar::Builder::new(BufWriter::new(tmp.file()));
    let meta_bytes = serde_json::to_vec(meta)?;
    append_entry(&mut tar, META_ENTRY, meta_bytes.len() as u64, meta_bytes.as_slice())?;
    for (actor, seqs) in &by_actor {
        for (first, lines) in log::line_runs(seqs.iter().map(|(&seq, &c)| (seq, c)))? {
            let mut seq = first;
            for chunk in lines.chunks(SEGMENT_LIMIT) {
                let bytes = chunk.concat();
                append_entry(&mut tar, &log::segment_rel_path(actor, seq), bytes.len() as u64, bytes.as_slice())?;
                seq += chunk.len() as u64;
            }
        }
    }
    for (id, path) in &blob_files {
        let f = File::open(path).map_err(|e| io_err(path, e))?;
        let size = f.metadata().map_err(|e| io_err(path, e))?.len();
        append_entry(&mut tar, &blob::rel_path(id)?, size, f.take(size))?;
    }
    let writer = tar.into_inner().map_err(|e| io_err(&tmp_path, e))?;
    writer.into_inner().map_err(|e| io_err(&tmp_path, e.into_error()))?;
    tmp.commit(out)
}

fn malformed(bundle: &Path, msg: impl std::fmt::Display) -> Error {
    Error::invalid(format!("bundle {}: {msg}", bundle.display()))
}

fn open_archive(bundle: &Path) -> Result<tar::Archive<BufReader<File>>> {
    match File::open(bundle) {
        Ok(f) => Ok(tar::Archive::new(BufReader::new(f))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(Error::not_found(format!("bundle {} not found", bundle.display())))
        }
        Err(e) => Err(io_err(bundle, e)),
    }
}

/// The path of a regular-file entry; `None` for directory entries, which some tar
/// producers emit and which carry no data. Any other entry type is rejected.
fn file_entry_path<R: Read>(entry: &tar::Entry<'_, R>, bundle: &Path) -> Result<Option<String>> {
    let kind = entry.header().entry_type();
    if kind.is_dir() {
        return Ok(None);
    }
    let path = String::from_utf8(entry.path_bytes().into_owned())
        .map_err(|_| malformed(bundle, "non-UTF-8 entry path"))?;
    if !kind.is_file() {
        return Err(malformed(bundle, format!("{path}: unsupported entry type {kind:?}")));
    }
    Ok(Some(path))
}

pub(crate) fn read_meta(bundle: &Path) -> Result<BundleMeta> {
    let mut archive = open_archive(bundle)?;
    for entry in archive.entries().map_err(|e| malformed(bundle, e))? {
        let mut entry = entry.map_err(|e| malformed(bundle, e))?;
        if file_entry_path(&entry, bundle)?.as_deref() != Some(META_ENTRY) {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| malformed(bundle, e))?;
        let meta: BundleMeta =
            serde_json::from_slice(&bytes).map_err(|e| malformed(bundle, format!("{META_ENTRY}: {e}")))?;
        validate_meta(&meta)?;
        return Ok(meta);
    }
    Err(malformed(bundle, format!("missing {META_ENTRY}")))
}

struct Parsed {
    meta: BundleMeta,
    claims: Vec<Claim>,
    blobs: BTreeSet<Id>,
}

/// First pass: parses everything and checks every blob hash; writes nothing.
fn read_bundle(bundle: &Path) -> Result<Parsed> {
    let mut archive = open_archive(bundle)?;
    let mut meta = None;
    let mut claims = Vec::new();
    let mut blobs = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for entry in archive.entries().map_err(|e| malformed(bundle, e))? {
        let mut entry = entry.map_err(|e| malformed(bundle, e))?;
        let Some(path) = file_entry_path(&entry, bundle)? else {
            continue;
        };
        if !seen.insert(path.clone()) {
            return Err(malformed(bundle, format!("{path}: duplicate entry")));
        }
        if path == META_ENTRY {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| malformed(bundle, e))?;
            let parsed: BundleMeta =
                serde_json::from_slice(&bytes).map_err(|e| malformed(bundle, format!("{META_ENTRY}: {e}")))?;
            meta = Some(parsed);
        } else if let Some((actor, first)) = log::parse_segment_path(&path) {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| malformed(bundle, e))?;
            if bytes.is_empty() {
                return Err(malformed(bundle, format!("{path}: empty segment")));
            }
            for (i, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
                let claim = log::parse_line(line, &|| format!("bundle {}: {path}:{}", bundle.display(), i + 1))?;
                let expected = first + i as u64;
                if claim.body.actor != actor || claim.body.seq != expected {
                    return Err(Error::integrity(format!(
                        "bundle {}: {path}:{}: claim {} (seq {} of {}) does not belong at seq {expected} of {actor}",
                        bundle.display(),
                        i + 1,
                        claim.id,
                        claim.body.seq,
                        claim.body.actor
                    )));
                }
                claims.push(claim);
            }
        } else if let Some(id) = blob::parse_rel_path(&path) {
            let (got, _) = blob::hash_reader(&mut entry, &|| format!("bundle {}: {path}", bundle.display()))?;
            if got != id {
                return Err(Error::integrity(format!("bundle {}: {path}: content hashes to {got}", bundle.display())));
            }
            blobs.insert(id);
        } else {
            return Err(malformed(bundle, format!("{path}: unexpected entry")));
        }
    }
    let meta = meta.ok_or_else(|| malformed(bundle, format!("missing {META_ENTRY}")))?;
    validate_meta(&meta)?;
    Ok(Parsed { meta, claims, blobs })
}

pub(crate) fn import(bundle: &Path, dest: &Path) -> Result<(Replica, BundleMeta, ImportReport)> {
    let parsed = read_bundle(bundle)?;
    let space = parsed.meta.space.clone();
    for c in &parsed.claims {
        log::check_claim(c, &space, ErrorCode::Integrity)?;
    }
    // The manifest's owner and genesis are only as good as this claim: a genesis
    // that is not a `knowa:owner` claim by that owner would let a bundle name anyone
    // as the owner of the space (DESIGN §7.3, as `Replica::create` checks).
    let genesis = parsed.claims.iter().find(|c| c.id == parsed.meta.genesis && c.body.actor == parsed.meta.owner);
    let Some(genesis) = genesis else {
        return Err(Error::integrity(format!(
            "bundle {} lacks its genesis claim {} (seq 1 of owner {})",
            bundle.display(),
            parsed.meta.genesis,
            parsed.meta.owner
        )));
    };
    if let Some(p) = genesis_problem(genesis) {
        return Err(Error::integrity(format!("bundle {}: {p}", bundle.display())));
    }
    let manifest = manifest_of(&parsed.meta);
    let manifest_path = dest.join(MANIFEST_FILE);
    let replica = if manifest_path.try_exists().map_err(|e| io_err(&manifest_path, e))? {
        let replica = Replica::open(dest)?;
        if *replica.manifest() != manifest {
            return Err(Error::conflict(format!(
                "{} holds a different replica (space {}) than bundle {} (space {})",
                dest.display(),
                replica.manifest().space,
                bundle.display(),
                manifest.space
            )));
        }
        replica
    } else {
        // Run the chain checks against an empty replica before anything exists on disk.
        log::plan_merge(&parsed.claims, |_| Ok(Chain::new()))?;
        Replica::create_replica(dest, &manifest)?
    };

    let lock = replica.lock()?;
    let plan = log::plan_merge(&parsed.claims, |actor| log::chain(replica.root(), &space, actor))?;
    // The marker covers blobs too: a text blob arriving for claims that are already
    // indexed changes their full-text documents, which catch-up never revisits.
    let adds_blobs = parsed.blobs.iter().any(|id| !replica.has_blob(id));
    if plan.writes_segments() || adds_blobs {
        mark_rebuild(replica.root())?;
    }

    // Blobs before claims: a failure part-way leaves no claim referring to a blob
    // that did not make it.
    let mut blobs_added = 0;
    if adds_blobs {
        let mut archive = open_archive(bundle)?;
        for entry in archive.entries().map_err(|e| malformed(bundle, e))? {
            let mut entry = entry.map_err(|e| malformed(bundle, e))?;
            let Some(path) = file_entry_path(&entry, bundle)? else {
                continue;
            };
            let Some(id) = blob::parse_rel_path(&path) else {
                continue;
            };
            if replica.has_blob(&id) {
                continue;
            }
            let (got, created, _) = blob::put(replica.root(), &mut entry)?;
            if got != id {
                return Err(Error::integrity(format!(
                    "bundle {} changed during import: {path} now hashes to {got}",
                    bundle.display()
                )));
            }
            if created {
                blobs_added += 1;
            }
        }
    }
    let mut report = plan.apply(replica.root())?;
    report.blobs_added = blobs_added;
    drop(lock);
    Ok((replica, parsed.meta, report))
}
