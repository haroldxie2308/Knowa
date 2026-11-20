//! Export selection (DESIGN §7.3).

use std::collections::{BTreeMap, HashSet};

use knowa_model::{Id, Result, Status, Visibility, vocab};
use rusqlite::Connection;

use crate::blobs::BlobSource;
use crate::db::{db, id_from, value_from};
use crate::policy::{self, Acceptance};
use crate::rules;

/// Trust claims (DESIGN §7.3 (b)): always exported, so a receiver can compute
/// statuses. Their notes are therefore effectively public.
const TRUST_PREDICATES: [&str; 5] = [vocab::OWNER, vocab::ROLE, vocab::AUTO_ACCEPT, vocab::ACTOR_KIND, vocab::DEFAULT_VISIBILITY];

/// Latest (at, id) effective setting per subject.
type Settings = BTreeMap<String, (i64, String, Visibility)>;

/// Effective visibility settings: `knowa:visibility` per subject and the space's
/// `knowa:defaultVisibility`, counting only non-retracted claims by owners.
fn settings(conn: &Connection, space: &Id, acc: &Acceptance) -> Result<(Settings, Option<Visibility>)> {
    let mut stmt = conn
        .prepare_cached("SELECT id, actor, at, s, p, o FROM claims WHERE p IN (?1, ?2) AND status != ?3")
        .map_err(db)?;
    let mut rows = stmt.query([vocab::VISIBILITY, vocab::DEFAULT_VISIBILITY, Status::Retracted.as_str()]).map_err(db)?;
    let mut subject = Settings::new();
    let mut default = Settings::new();
    while let Some(row) = rows.next().map_err(db)? {
        let id: String = row.get(0).map_err(db)?;
        let actor: String = row.get(1).map_err(db)?;
        let at: i64 = row.get(2).map_err(db)?;
        let s: String = row.get(3).map_err(db)?;
        let p: String = row.get(4).map_err(db)?;
        let Some(vis) = rules::visibility_of(&value_from(&row.get::<_, String>(5).map_err(db)?)?) else { continue };
        if !acc.is_owner(&actor) {
            continue;
        }
        let target = if p == vocab::DEFAULT_VISIBILITY {
            if s != space.as_str() {
                continue;
            }
            &mut default
        } else {
            &mut subject
        };
        if target.get(&s).is_none_or(|(a, i, _)| (at, &id) > (*a, i)) {
            target.insert(s, (at, id, vis));
        }
    }
    let default = default.into_values().next().map(|(_, _, v)| v);
    Ok((subject, default))
}

/// (claims ordered by actor then seq, blobs ordered by id).
///
/// - `Private`: every claim and every blob present (DESIGN §7.3: everything).
/// - `Shared` / `Public` = L: (a) non-policy claims whose effective visibility (own
///   setting, else the subject's, else the space default, else Private) ≥ L; (b) trust
///   claims; (c) review/retract claims whose target is exported, transitively.
///   `knowa:visibility` claims never. Blobs by their own effective visibility only
///   (setting on the bare `b_` id, else the space default, else Private) ≥ L, and
///   present: being referenced by an exported claim is not enough.
pub(crate) fn export_set(conn: &Connection, space: &Id, level: Visibility, blobs: &dyn BlobSource) -> Result<(Vec<Id>, Vec<Id>)> {
    if level == Visibility::Private {
        return everything(conn, blobs);
    }
    let acc = Acceptance::of(&policy::load(conn)?);
    let (by_subject, default) = settings(conn, space, &acc)?;
    // DESIGN §4.6: an absent defaultVisibility means knowa:Private.
    let default = default.unwrap_or(Visibility::Private);
    // The setting on `key` itself, if any. Callers fall back along DESIGN §7.3's chain
    // (claim → subject → space default): an unset id is not an error, it inherits.
    let visible = |key: &str| by_subject.get(key).map(|(_, _, v)| *v);

    // (a) and (b)
    let mut exported: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare_cached("SELECT id, s, p, policy FROM claims").map_err(db)?;
    let mut rows = stmt.query([]).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        let id: String = row.get(0).map_err(db)?;
        let s: String = row.get(1).map_err(db)?;
        let p: String = row.get(2).map_err(db)?;
        let is_policy: bool = row.get(3).map_err(db)?;
        let export = if TRUST_PREDICATES.contains(&p.as_str()) {
            true
        } else if is_policy {
            // review/retract: (c) below; visibility: never
            false
        } else {
            visible(&id).or_else(|| visible(&s)).unwrap_or(default) >= level
        };
        if export {
            exported.insert(id);
        }
    }

    // (c): reviews and retracts travel with their target, so a retract of an exported
    // retract is exported too. Each has one target, so this walks a forest.
    let mut frontier: Vec<String> = exported.iter().cloned().collect();
    let mut on = conn.prepare_cached("SELECT id FROM claims WHERE s = ?1 AND p IN (?2, ?3)").map_err(db)?;
    while let Some(target) = frontier.pop() {
        let mut rows = on.query([target.as_str(), vocab::REVIEW, vocab::RETRACT]).map_err(db)?;
        while let Some(row) = rows.next().map_err(db)? {
            let id: String = row.get(0).map_err(db)?;
            if exported.insert(id.clone()) {
                frontier.push(id);
            }
        }
    }

    let mut claims = Vec::with_capacity(exported.len());
    let mut stmt = conn.prepare_cached("SELECT id FROM claims ORDER BY actor, seq").map_err(db)?;
    let mut rows = stmt.query([]).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        let id: String = row.get(0).map_err(db)?;
        if exported.contains(&id) {
            claims.push(id_from(&id)?);
        }
    }

    let mut out_blobs = Vec::new();
    for b in blobs.blob_ids()? {
        if visible(b.as_str()).unwrap_or(default) >= level {
            out_blobs.push(b);
        }
    }
    out_blobs.sort();
    Ok((claims, out_blobs))
}

/// The private-level export.
fn everything(conn: &Connection, blobs: &dyn BlobSource) -> Result<(Vec<Id>, Vec<Id>)> {
    let mut claims = Vec::new();
    let mut stmt = conn.prepare_cached("SELECT id FROM claims ORDER BY actor, seq").map_err(db)?;
    let mut rows = stmt.query([]).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        claims.push(id_from(&row.get::<_, String>(0).map_err(db)?)?);
    }
    let mut all = blobs.blob_ids()?;
    all.sort();
    Ok((claims, all))
}
