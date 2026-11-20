//! Export selection (DESIGN §7.3).

use std::collections::{BTreeMap, BTreeSet};

use knowa_model::{Id, Result, Status, Visibility, vocab};
use rusqlite::Connection;

use crate::blobs::BlobSource;
use crate::db::{db, id_from, value_from};
use crate::policy::{self, Acceptance};
use crate::rules;

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
pub(crate) fn export_set(conn: &Connection, space: &Id, level: Visibility, blobs: &dyn BlobSource) -> Result<(Vec<Id>, Vec<Id>)> {
    let acc = Acceptance::of(&policy::load(conn)?);
    let (by_subject, default) = settings(conn, space, &acc)?;
    // DESIGN §4.6: an absent defaultVisibility means knowa:Private.
    let default = default.unwrap_or(Visibility::Private);

    let mut claims = Vec::new();
    let mut refs: BTreeSet<Id> = BTreeSet::new();
    let mut stmt = conn.prepare_cached("SELECT id, s, policy FROM claims ORDER BY actor, seq").map_err(db)?;
    let mut rows = stmt.query([]).map_err(db)?;
    let mut blob_refs = conn.prepare_cached("SELECT blob FROM blob_refs WHERE claim = ?1").map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        let id: String = row.get(0).map_err(db)?;
        let s: String = row.get(1).map_err(db)?;
        let is_policy: bool = row.get(2).map_err(db)?;
        let vis = by_subject.get(&id).or_else(|| by_subject.get(&s)).map_or(default, |(_, _, v)| *v);
        if !(is_policy || vis >= level) {
            continue;
        }
        let mut brows = blob_refs.query([id.as_str()]).map_err(db)?;
        while let Some(b) = brows.next().map_err(db)? {
            refs.insert(id_from(&b.get::<_, String>(0).map_err(db)?)?);
        }
        claims.push(id_from(&id)?);
    }
    let blobs = refs.into_iter().filter(|b| blobs.has_blob(b)).collect();
    Ok((claims, blobs))
}
