//! Admin ops: init, export, import, verify, reindex (DESIGN §5, §7, §7.3).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use knowa_config::{Home, SpaceRef, validate_name};
use knowa_index::Index;
use knowa_model::query::View;
use knowa_model::{ClaimBody, Draft, Error, ErrorCode, Id, Result, Timestamp, Value, Visibility, vocab};
use knowa_store::{
    BundleMeta, Location, Manifest, Replica, VerifyReport, import_bundle, read_bundle_meta, write_bundle,
};

use crate::select::{Space, blob_cache, current, index_dir, open_replica};
use crate::write::{Txn, term};
use crate::{ExportResult, ImportResult, InitResult, Sel, SpaceAddResult};

pub(crate) fn init(
    home: &Home,
    sel: &Sel,
    location: &Location,
    blobs: Option<&Location>,
    name: &str,
    alias: Option<&str>,
) -> Result<InitResult> {
    if name.trim().is_empty() {
        return Err(Error::invalid("init: --name must not be empty"));
    }
    let alias = match alias {
        Some(a) => {
            validate_name(a)?;
            a.to_string()
        }
        None => default_alias("init", location)?,
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
    let replica = Replica::create(location, blobs, &blob_cache(home), &genesis)?;
    let (location, blobs) = registered_locations(location, blobs)?;
    let sref = SpaceRef { alias: Some(alias.clone()), location: location.clone(), blobs: blobs.clone(), readonly: false };
    let space = Space::new(home, sref, replica)?;

    // The owner's actorKind comes from the write prelude.
    let written = Txn::begin(&space, &[&owner])?.write(
        &owner,
        vec![
            Draft::new(space_id.clone(), term(vocab::LABEL)?, Value::text(name)),
            Draft::new(extract.id.clone(), term(vocab::ROLE)?, Value::Term(term(vocab::ROLE_WORKER)?)),
            Draft::new(extract.id.clone(), term(vocab::AUTO_ACCEPT)?, Value::Bool(true)),
        ],
    )?;
    // Registered only once the space is complete: a failure above leaves an
    // unregistered replica, never a registered half-initialized space.
    home.register_space(&alias, &location, blobs.as_deref(), &space_id, false)?;
    if home.load_config()?.current_space.is_none() {
        home.set_current_space(&alias)?;
    }

    let mut claims = vec![genesis.id];
    claims.extend(written.prelude);
    claims.extend(written.result.items.into_iter().map(|i| i.id));
    Ok(InitResult { space: space_id, location, blobs, alias, owner: owner.id, extract_actor: extract.id, claims })
}

/// The canonical replica and blob locations a registration stores (DESIGN §5.2). A
/// blob location equal to the replica's is the default, so it is stored as none: every
/// registration of the replica then reads the same.
fn registered_locations(location: &Location, blobs: Option<&Location>) -> Result<(String, Option<String>)> {
    let location = location.canonical()?;
    let blobs = blobs.map(Location::canonical).transpose()?.filter(|b| *b != location);
    Ok((location, blobs))
}

/// The last name in `location`, which must already be a valid alias: a directory's
/// base name, an object location's last prefix segment, else its bucket. `op` names
/// the command, for messages.
fn default_alias(op: &str, location: &Location) -> Result<String> {
    let base = match location {
        Location::Dir(dir) => std::path::absolute(dir)?.file_name().and_then(|n| n.to_str()).map(str::to_string),
        Location::Object(url) => url.trim_end_matches('/').rsplit('/').next().map(str::to_string),
    };
    let base = base.ok_or_else(|| Error::invalid(format!("{op}: cannot derive an alias from {location}; pass --alias")))?;
    validate_name(&base).map_err(|e| {
        Error::invalid(format!("{op}: the location's name {base:?} is not a valid alias ({}); pass --alias", e.message))
    })?;
    Ok(base)
}

pub(crate) fn export(home: &Home, sel: &Sel, out: &Path, level: Visibility) -> Result<ExportResult> {
    let space = current(home, sel)?;
    let actor = home.resolve_actor(sel.actor.as_deref())?;
    let index = space.index()?;
    let (claim_ids, blobs) = index.export_set(&space.replica, level)?;
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
    let meta = read_bundle_meta(bundle)?;
    let claimed = Manifest {
        format: meta.format,
        space: meta.space.clone(),
        owner: meta.owner.clone(),
        genesis: meta.genesis.clone(),
    };
    // Before anything is written. `import_bundle` checks that the genesis claim backs
    // these fields, so a bundle cannot pass this check under someone else's identity.
    check_identity(home, &claimed, "import")?;
    let dest = match dir {
        Some(d) => d.to_path_buf(),
        None => home.root().join("replicas").join(meta.space.as_str()),
    };
    refuse_writable(home, &dest)?;
    let (replica, imported, report) = import_bundle(bundle, &dest)?;
    if imported != meta {
        return Err(Error::integrity(format!(
            "import: bundle {} changed while it was being imported into {}; nothing was registered",
            bundle.display(),
            dest.display()
        )));
    }
    let location = replica.location().canonical()?;
    // If the merge added anything it wrote a new rebuild generation, and `Index::open`
    // rebuilds (claims that fill gaps below an indexed head are invisible to catch-up);
    // if it added nothing, catching up is all there is to do (DESIGN §5.4).
    let index = Index::open(&replica, &index_dir(home, &replica)?)?;
    let alias = match alias {
        Some(a) => a.to_string(),
        None => default_import_alias(home, &location, &index, &meta.space)?,
    };
    home.register_space(&alias, &location, None, &meta.space, true).map_err(|mut e| {
        e.message = format!("imported into {location}, but registering it failed: {} (re-run with --alias)", e.message);
        e
    })?;
    Ok(ImportResult {
        space: meta.space,
        path: PathBuf::from(location),
        alias,
        added: report.added,
        skipped_existing: report.skipped_existing,
        blobs_added: report.blobs_added,
        gaps: report.gaps.len() as u64,
    })
}

/// DESIGN §7.3: a space is identified by its id *and* genesis. Refuses (`conflict`)
/// when a registered replica of `m.space` has a different owner or genesis: that is
/// another space reusing the id (e.g. a bundle signed by someone else for a known
/// space id), and registering it would let it stand in for the real one.
fn check_identity(home: &Home, m: &Manifest, op: &str) -> Result<()> {
    for (alias, entry) in home.load_config()?.spaces {
        if entry.space != m.space {
            continue;
        }
        let other = open_replica(home, &entry.location, entry.blobs.as_deref()).map_err(|mut e| {
            e.message = format!("{op}: registered space '{alias}' (same space id): {}", e.message);
            e
        })?;
        let known = other.manifest();
        if known != m {
            return Err(Error::conflict(format!(
                "{op}: space {} is registered as '{alias}' with owner {} and genesis {}, but this replica has owner {} and genesis {}; a space is its id and genesis, so this is a different space reusing the id",
                m.space, known.owner, known.genesis, m.owner, m.genesis
            ))
            .with_detail(serde_json::json!({ "alias": alias, "registered": known, "incoming": m })));
        }
    }
    Ok(())
}

/// Import registers its destination readonly; refuse a destination registered as a
/// writable space, which the import would otherwise silently make readonly.
fn refuse_writable(home: &Home, dir: &Path) -> Result<()> {
    let canon = match Location::Dir(dir.to_path_buf()).canonical() {
        Ok(c) => c,
        // Not created yet, so it cannot be registered.
        Err(e) if e.code == ErrorCode::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for (alias, entry) in home.load_config()?.spaces {
        if entry.location == canon && !entry.readonly {
            return Err(Error::conflict(format!(
                "import: {canon} is your writable space '{alias}'; import into another directory"
            )));
        }
    }
    Ok(())
}

/// Existing registration of this location, else the space's label slugified, else the
/// space id.
fn default_import_alias(home: &Home, location: &str, index: &Index, space: &Id) -> Result<String> {
    if let Some((alias, _)) = home.load_config()?.spaces.into_iter().find(|(_, e)| e.location == location) {
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

/// Registers an existing replica (DESIGN §7 `space add`), with its blobs at `blobs`.
/// Default alias: the space's label slugified, else the location's name (as `init`).
pub(crate) fn space_add(
    home: &Home,
    location: &Location,
    blobs: Option<&Location>,
    alias: Option<&str>,
    readonly: bool,
) -> Result<SpaceAddResult> {
    let replica = Replica::open(location, blobs, &blob_cache(home))?;
    check_identity(home, replica.manifest(), "space add")?;
    let (canonical, blobs) = registered_locations(location, blobs)?;
    let space = replica.manifest().space.clone();
    let alias = match alias {
        Some(a) => {
            validate_name(a)?;
            a.to_string()
        }
        None => {
            let index = Index::open(&replica, &index_dir(home, &replica)?)?;
            let label = index.entity(&space, &View::default())?.and_then(|e| e.label);
            match label.as_deref().and_then(slugify) {
                Some(a) => a,
                None => default_alias("space add", location)?,
            }
        }
    };
    home.register_space(&alias, &canonical, blobs.as_deref(), &space, readonly)?;
    Ok(SpaceAddResult { alias, space, location: canonical, blobs, readonly })
}

/// Each actor is verified under its exclusive lock (`Replica::verify`): a concurrent
/// append's half-written line would otherwise be reported as a torn line, and taking
/// the lock first repairs a real torn tail (DESIGN §5.3), so verify reports the
/// replica as every later reader will see it.
pub(crate) fn verify(home: &Home, sel: &Sel) -> Result<VerifyReport> {
    current(home, sel)?.replica.verify()
}

/// Rebuilds under the index lock (no actor lock: DESIGN §5.3); returns the new epoch.
pub(crate) fn reindex(home: &Home, sel: &Sel) -> Result<String> {
    let space = current(home, sel)?;
    Ok(Index::rebuild(&space.replica, &space.index_dir)?.epoch().to_string())
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
