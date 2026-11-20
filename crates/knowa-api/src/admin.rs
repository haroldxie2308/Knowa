//! Admin ops: init, export, import, verify, reindex (DESIGN §5, §7, §7.3).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use knowa_config::{Home, SpaceRef, validate_name};
use knowa_index::Index;
use knowa_model::query::View;
use knowa_model::{ClaimBody, Draft, Error, Id, Result, Timestamp, Value, Visibility, vocab};
use knowa_store::{BundleMeta, ImportReport, Replica, VerifyReport, import_bundle, read_bundle_meta, write_bundle};

use crate::select::{Space, current};
use crate::write::{Txn, term};
use crate::{ExportResult, ImportResult, InitResult, Sel, SpaceAddResult};

pub(crate) fn init(home: &Home, sel: &Sel, dir: &Path, name: &str, alias: Option<&str>) -> Result<InitResult> {
    if name.trim().is_empty() {
        return Err(Error::invalid("init: --name must not be empty"));
    }
    let alias = match alias {
        Some(a) => {
            validate_name(a)?;
            a.to_string()
        }
        None => default_init_alias(dir)?,
    };
    if home.load_config()?.spaces.contains_key(&alias) {
        return Err(Error::conflict(format!("init: alias '{alias}' is already registered; pass --alias")));
    }
    let owner = home.resolve_actor(sel.actor.as_deref())?;
    let extract = home.extract_actor()?;

    let space_id = Id::new_space()?;
    let genesis = owner.signer.seal(ClaimBody::from_draft(
        Draft::new(space_id.clone(), term(vocab::OWNER)?, Value::Ref(owner.id.clone())),
        space_id.clone(),
        owner.id.clone(),
        1,
        None,
        Timestamp::now(),
    ))?;
    let replica = Replica::create(dir, &genesis)?;
    let path = fs::canonicalize(replica.root())?;
    let space = Space { sref: SpaceRef { alias: Some(alias.clone()), path: path.clone(), readonly: false }, replica };

    // The owner's actorKind comes from the write prelude.
    let written = Txn::begin(&space)?.write(
        &owner,
        vec![
            Draft::new(space_id.clone(), term(vocab::LABEL)?, Value::text(name)),
            Draft::new(extract.id.clone(), term(vocab::ROLE)?, Value::Term(term(vocab::ROLE_WORKER)?)),
            Draft::new(extract.id.clone(), term(vocab::AUTO_ACCEPT)?, Value::Bool(true)),
        ],
    )?;
    // Registered only once the space is complete: a failure above leaves an
    // unregistered directory, never a registered half-initialized space.
    home.register_space(&alias, &path, &space_id, false)?;
    if home.load_config()?.current_space.is_none() {
        home.set_current_space(&alias)?;
    }

    let mut claims = vec![genesis.id];
    claims.extend(written.prelude);
    claims.extend(written.result.items.into_iter().map(|i| i.id));
    Ok(InitResult { space: space_id, path, alias, owner: owner.id, extract_actor: extract.id, claims })
}

/// The directory's base name, which must already be a valid alias.
fn default_init_alias(dir: &Path) -> Result<String> {
    let abs = std::path::absolute(dir)?;
    let base = abs
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::invalid(format!("init: cannot derive an alias from {}; pass --alias", dir.display())))?;
    validate_name(base).map_err(|e| {
        Error::invalid(format!("init: the directory name {base:?} is not a valid alias ({}); pass --alias", e.message))
    })?;
    Ok(base.to_string())
}

pub(crate) fn export(home: &Home, sel: &Sel, out: &Path, level: Visibility) -> Result<ExportResult> {
    let space = current(home, sel)?;
    let actor = home.resolve_actor(sel.actor.as_deref())?;
    let index = space.index()?;
    let (claim_ids, blobs) = index.export_set(level)?;
    let mut wanted: HashSet<Id> = claim_ids.into_iter().collect();
    // Claims are read from the logs, the canonical copy (DESIGN §3.7).
    let mut claims = Vec::with_capacity(wanted.len());
    for actor_id in space.replica.actors()? {
        for c in space.replica.read_log(&actor_id, 1)? {
            let c = c?;
            if wanted.remove(&c.id) {
                claims.push(c);
            }
        }
    }
    if !wanted.is_empty() {
        let mut missing: Vec<&Id> = wanted.iter().collect();
        missing.sort();
        return Err(Error::integrity(format!(
            "export: the index selected {} claims that are not in the logs; run knowa reindex",
            missing.len()
        ))
        .with_detail(serde_json::json!({ "missing": missing })));
    }
    let m = space.replica.manifest();
    let partial = level != Visibility::Private;
    let meta = BundleMeta {
        format: vocab::FORMAT_VERSION,
        space: m.space.clone(),
        owner: m.owner.clone(),
        genesis: m.genesis.clone(),
        level: level_name(level).to_string(),
        partial,
        exported_by: actor.id,
        at: Timestamp::now().format(),
    };
    write_bundle(&space.replica, out, &meta, &claims, &blobs)?;
    Ok(ExportResult {
        path: std::path::absolute(out)?,
        level,
        partial,
        claims: claims.len() as u64,
        blobs: blobs.len() as u64,
    })
}

fn level_name(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => "private",
        Visibility::Shared => "shared",
        Visibility::Public => "public",
    }
}

pub(crate) fn import(home: &Home, bundle: &Path, dir: Option<&Path>, alias: Option<&str>) -> Result<ImportResult> {
    fs::metadata(bundle).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::not_found(format!("import: no such bundle: {}", bundle.display())),
        _ => Error::from(e),
    })?;
    if let Some(a) = alias {
        validate_name(a)?;
    }
    let (replica, meta, report) = match dir {
        Some(d) => {
            refuse_writable(home, d)?;
            import_bundle(bundle, d)?
        }
        None => import_default(home, bundle)?,
    };
    let path = fs::canonicalize(replica.root())?;
    // Merged claims can fill seq gaps below an actor's indexed head, which catch-up
    // (claims above the head) would miss, so the index is rebuilt (DESIGN §7.3).
    let index = {
        let _lock = replica.lock()?;
        Index::rebuild(&replica)?
    };
    let alias = match alias {
        Some(a) => a.to_string(),
        None => default_import_alias(home, &path, &index, &meta.space)?,
    };
    home.register_space(&alias, &path, &meta.space, true).map_err(|mut e| {
        e.message = format!("imported into {}, but registering it failed: {} (re-run with --alias)", path.display(), e.message);
        e
    })?;
    Ok(ImportResult {
        space: meta.space,
        path,
        alias,
        added: report.added,
        skipped_existing: report.skipped_existing,
        blobs_added: report.blobs_added,
        gaps: report.gaps.len() as u64,
    })
}

/// Default destination `<home>/replicas/<space-id>`, chosen from the bundle's meta.
fn import_default(home: &Home, bundle: &Path) -> Result<(Replica, BundleMeta, ImportReport)> {
    let meta = read_bundle_meta(bundle)?;
    let dest = home.root().join("replicas").join(meta.space.as_str());
    refuse_writable(home, &dest)?;
    import_bundle(bundle, &dest)
}

/// Import registers its destination readonly; refuse a destination registered as a
/// writable space, which the import would otherwise silently make readonly.
fn refuse_writable(home: &Home, dir: &Path) -> Result<()> {
    let canon = match fs::canonicalize(dir) {
        Ok(c) => c,
        // Not created yet, so it cannot be registered.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for (alias, entry) in home.load_config()?.spaces {
        if entry.path == canon && !entry.readonly {
            return Err(Error::conflict(format!(
                "import: {} is your writable space '{alias}'; import into another directory",
                canon.display()
            )));
        }
    }
    Ok(())
}

/// Existing registration of this path, else the space's label slugified, else the
/// space id.
fn default_import_alias(home: &Home, path: &Path, index: &Index, space: &Id) -> Result<String> {
    if let Some((alias, _)) = home.load_config()?.spaces.into_iter().find(|(_, e)| e.path == *path) {
        return Ok(alias);
    }
    let label = index.entity(space, &View::default())?.and_then(|e| e.label);
    Ok(label.as_deref().and_then(slugify).unwrap_or_else(|| space.as_str().to_string()))
}

/// Lowercase `[a-z0-9._-]`, other characters → `-`, dash runs collapsed, must start
/// with `[a-z0-9]`, at most 64 characters (the alias grammar). `None` if nothing is left.
pub(crate) fn slugify(label: &str) -> Option<String> {
    let mut s = String::new();
    for c in label.chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' { c } else { '-' };
        if c == '-' && s.ends_with('-') {
            continue;
        }
        if s.is_empty() && !(c.is_ascii_lowercase() || c.is_ascii_digit()) {
            continue;
        }
        s.push(c);
    }
    s.truncate(64);
    let s = s.trim_end_matches('-').to_string();
    (!s.is_empty()).then_some(s)
}

/// Registers an existing replica directory (DESIGN §7 `space add`). Default alias: the
/// space's label slugified, else the directory name (as `init`).
pub(crate) fn space_add(home: &Home, dir: &Path, alias: Option<&str>, readonly: bool) -> Result<SpaceAddResult> {
    let replica = Replica::open(dir)?;
    let path = fs::canonicalize(replica.root())?;
    let space = replica.manifest().space.clone();
    let alias = match alias {
        Some(a) => {
            validate_name(a)?;
            a.to_string()
        }
        None => {
            let index = {
                let _lock = replica.lock()?;
                Index::open(&replica)?
            };
            let label = index.entity(&space, &View::default())?.and_then(|e| e.label);
            match label.as_deref().and_then(slugify) {
                Some(a) => a,
                None => default_init_alias(dir)?,
            }
        }
    };
    home.register_space(&alias, &path, &space, readonly)?;
    Ok(SpaceAddResult { alias, space, path, readonly })
}

pub(crate) fn verify(home: &Home, sel: &Sel) -> Result<VerifyReport> {
    current(home, sel)?.replica.verify()
}

/// Returns the new index epoch.
pub(crate) fn reindex(home: &Home, sel: &Sel) -> Result<String> {
    let space = current(home, sel)?;
    let _lock = space.replica.lock()?;
    Ok(Index::rebuild(&space.replica)?.epoch().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_labels() {
        assert_eq!(slugify("Alice's Photos").as_deref(), Some("alice-s-photos"));
        assert_eq!(slugify("  --Trip 2024!! ").as_deref(), Some("trip-2024"));
        assert_eq!(slugify("notes.v2_final").as_deref(), Some("notes.v2_final"));
        assert_eq!(slugify("日記"), None);
        assert_eq!(slugify("---"), None);
        assert_eq!(slugify("_x").as_deref(), Some("x"));
    }

    #[test]
    fn slugify_caps_length() {
        let s = slugify(&"a".repeat(100)).unwrap();
        assert_eq!(s.len(), 64);
        let t = slugify(&format!("{}-{}", "a".repeat(63), "b")).unwrap();
        assert_eq!(t, "a".repeat(63));
    }
}
