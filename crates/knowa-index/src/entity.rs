//! Entity rows: summary fields (DESIGN §4.7 display value, §4.8 when), the FTS
//! document and the geo point, recomputed from the entity's claims.

use std::collections::BTreeSet;

use knowa_model::view::{EntityWhen, EntityWhere, WhenSource};
use knowa_model::{EntityView, Id, IdKind, Result, Timestamp, Value, vocab};
use rusqlite::Connection;

use crate::blobs::{self, BlobSource};
use crate::db::{corrupt, db, id_from, status_from, term_from, value_from};
use crate::fts;
use crate::rules;

/// An accepted|proposed claim, for display ordering.
struct Active {
    id: String,
    at: i64,
    accepted: bool,
    p: String,
    o: Value,
}

/// Display order (DESIGN §4.7): accepted before proposed, then latest `at`; the id
/// breaks ties so the choice is deterministic.
fn display_sort(v: &mut [Active]) {
    v.sort_by(|a, b| b.accepted.cmp(&a.accepted).then(b.at.cmp(&a.at)).then(b.id.cmp(&a.id)));
}

/// Accepted|proposed claims with subject `s` (and predicate `p` if given), in display order.
fn active_claims(conn: &Connection, s: &str, p: Option<&str>) -> Result<(Vec<Active>, Option<i64>)> {
    let mut stmt = match p {
        Some(_) => conn.prepare_cached("SELECT id, p, o, at, status FROM claims WHERE s = ?1 AND p = ?2 AND ent = 1"),
        None => conn.prepare_cached("SELECT id, p, o, at, status FROM claims WHERE s = ?1 AND ent = 1"),
    }
    .map_err(db)?;
    let mut rows = match p {
        Some(p) => stmt.query(rusqlite::params![s, p]),
        None => stmt.query(rusqlite::params![s]),
    }
    .map_err(db)?;
    let mut active = Vec::new();
    let mut first_at: Option<i64> = None;
    while let Some(row) = rows.next().map_err(db)? {
        let at: i64 = row.get(3).map_err(db)?;
        first_at = Some(first_at.map_or(at, |f| f.min(at)));
        let status = status_from(&row.get::<_, String>(4).map_err(db)?)?;
        if !rules::is_active(status) {
            continue;
        }
        let o: String = row.get(2).map_err(db)?;
        active.push(Active {
            id: row.get(0).map_err(db)?,
            at,
            accepted: status == knowa_model::Status::Accepted,
            p: row.get(1).map_err(db)?,
            o: value_from(&o)?,
        });
    }
    display_sort(&mut active);
    Ok((active, first_at))
}

/// Display-chosen accepted|proposed `knowa:mediaType` of `blob` (base id).
pub(crate) fn media_type(conn: &Connection, blob: &str) -> Result<Option<String>> {
    let (active, _) = active_claims(conn, blob, Some(vocab::MEDIA_TYPE))?;
    Ok(active.into_iter().find_map(|a| match a.o {
        Value::Text { text, .. } => Some(text),
        _ => None,
    }))
}

fn is_text_blob(conn: &Connection, blob: &str) -> Result<bool> {
    Ok(media_type(conn, blob)?.is_some_and(|m| m.to_ascii_lowercase().starts_with("text/")))
}

/// Entities whose FTS document includes `blob`'s text through `knowa:content`.
pub(crate) fn content_referrers(conn: &Connection, blob: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare_cached("SELECT DISTINCT s FROM claims WHERE o_ref_base = ?1 AND p = ?2 AND ent = 1")
        .map_err(db)?;
    let mut rows = stmt.query(rusqlite::params![blob, vocab::CONTENT]).map_err(db)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db)? {
        out.push(row.get(0).map_err(db)?);
    }
    Ok(out)
}

/// Recomputes the row, FTS document and geo point of `entity`, which must be the
/// subject of at least one entity-materializing claim.
pub(crate) fn refresh(conn: &Connection, blobs: &dyn BlobSource, entity: &str) -> Result<()> {
    let (active, first_at) = active_claims(conn, entity, None)?;
    let first_at = first_at.ok_or_else(|| corrupt(format_args!("entity {entity} has no claims")))?;

    let mut supporting: Vec<&str> = Vec::new();
    let label = active.iter().find_map(|a| match &a.o {
        Value::Text { text, .. } if a.p == vocab::LABEL => Some((text.as_str(), a.id.as_str())),
        _ => None,
    });
    if let Some((_, id)) = label {
        supporting.push(id);
    }
    let mut types: Vec<&str> = Vec::new();
    for a in &active {
        if let Value::Term(t) = &a.o
            && a.p == vocab::TYPE
            && !types.contains(&t.as_str())
        {
            types.push(t.as_str());
            supporting.push(&a.id);
        }
    }

    // when: the display-chosen knowa:when, else the earliest record time (§4.8)
    struct WhenRow {
        value: String,
        lo: Option<i64>,
        hi: Option<i64>,
        source: &'static str,
        key: Option<i64>,
    }
    let claimed = active.iter().find_map(|a| match &a.o {
        Value::When(w) if a.p == vocab::WHEN => Some((w, a.id.as_str())),
        _ => None,
    });
    let when = if let Some((w, id)) = claimed {
        supporting.push(id);
        // Open-ended ranges have no midpoint; they sort by their finite end.
        let key = w.center().or(w.lo).or(w.hi);
        Some(WhenRow { value: w.raw.clone(), lo: w.lo, hi: w.hi, source: "claim", key })
    } else if let Some(earliest) = active.iter().min_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id))) {
        supporting.push(&earliest.id);
        let at = earliest.at;
        Some(WhenRow { value: Timestamp(at).format(), lo: Some(at), hi: Some(at + 1), source: "recorded", key: Some(at) })
    } else {
        None
    };

    let place = active.iter().find_map(|a| match &a.o {
        Value::Geo(g) if a.p == vocab::WHERE => Some((g, a.id.as_str())),
        _ => None,
    });
    let point = place.map(|(g, id)| {
        supporting.push(id);
        (g.lat.to_f64(), g.lon.to_f64(), g.r.as_ref().map(|r| r.to_f64()))
    });

    let mut seen = BTreeSet::new();
    supporting.retain(|id| seen.insert(*id));

    let n: i64 = conn
        .prepare_cached(
            "INSERT INTO entities(id, first_at, label, types, when_value, when_lo, when_hi, when_source, when_key, lat, lon, r, claims)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET first_at = excluded.first_at, label = excluded.label, types = excluded.types,
               when_value = excluded.when_value, when_lo = excluded.when_lo, when_hi = excluded.when_hi,
               when_source = excluded.when_source, when_key = excluded.when_key,
               lat = excluded.lat, lon = excluded.lon, r = excluded.r, claims = excluded.claims
             RETURNING n",
        )
        .map_err(db)?
        .query_row(
            rusqlite::params![
                entity,
                first_at,
                label.map(|(l, _)| l),
                serde_json::json!(types).to_string(),
                when.as_ref().map(|w| w.value.as_str()),
                when.as_ref().and_then(|w| w.lo),
                when.as_ref().and_then(|w| w.hi),
                when.as_ref().map(|w| w.source),
                when.as_ref().and_then(|w| w.key),
                point.map(|p| p.0),
                point.map(|p| p.1),
                point.and_then(|p| p.2),
                serde_json::json!(supporting).to_string(),
            ],
            |r| r.get(0),
        )
        .map_err(db)?;

    conn.prepare_cached("DELETE FROM entity_types WHERE entity = ?1").map_err(db)?.execute([n]).map_err(db)?;
    let mut ins = conn.prepare_cached("INSERT INTO entity_types(type, entity) VALUES (?1, ?2)").map_err(db)?;
    for t in &types {
        ins.execute(rusqlite::params![t, n]).map_err(db)?;
    }

    conn.prepare_cached("DELETE FROM geo WHERE n = ?1").map_err(db)?.execute([n]).map_err(db)?;
    if let Some((lat, lon, _)) = point {
        conn.prepare_cached("INSERT INTO geo(n, min_lat, max_lat, min_lon, max_lon) VALUES (?1, ?2, ?2, ?3, ?3)")
            .map_err(db)?
            .execute(rusqlite::params![n, lat, lon])
            .map_err(db)?;
    }

    let body = fts::document(&fts_body(conn, blobs, entity, &active)?);
    conn.prepare_cached("DELETE FROM fts WHERE rowid = ?1").map_err(db)?.execute([n]).map_err(db)?;
    conn.prepare_cached("INSERT INTO fts(rowid, body) VALUES (?1, ?2)")
        .map_err(db)?
        .execute(rusqlite::params![n, body])
        .map_err(db)?;
    Ok(())
}

/// Text-valued objects, type names and text-blob content (the entity itself if it
/// is a bare text blob, and blobs reached via `knowa:content`).
fn fts_body(conn: &Connection, blobs: &dyn BlobSource, entity: &str, active: &[Active]) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut text_blobs: BTreeSet<Id> = BTreeSet::new();
    let me = id_from(entity)?;
    if me.kind() == IdKind::Blob && me.fragment().is_none() && is_text_blob(conn, entity)? {
        text_blobs.insert(me);
    }
    for a in active {
        match &a.o {
            Value::Text { text, .. } => parts.push(text.clone()),
            Value::Term(t) if a.p == vocab::TYPE => parts.push(t.name().to_string()),
            Value::Ref(r) if a.p == vocab::CONTENT && r.kind() == IdKind::Blob => {
                let base = r.base();
                if is_text_blob(conn, base.as_str())? {
                    text_blobs.insert(base);
                }
            }
            _ => {}
        }
    }
    for b in &text_blobs {
        if let Some(text) = blobs::fts_text(blobs, b)? {
            parts.push(text);
        }
    }
    Ok(parts.join("\n"))
}

/// Columns read by `view_at`, for an `entities` table aliased `e`.
pub(crate) const COLS: &str = "e.id, e.label, e.types, e.when_value, e.when_lo, e.when_hi, e.when_source, e.lat, e.lon, e.r, e.claims";
pub(crate) const NCOLS: usize = 11;

/// Builds an `EntityView` from `COLS` at columns `at..at+NCOLS`.
pub(crate) fn view_at(row: &rusqlite::Row<'_>, at: usize, space: &Id) -> Result<EntityView> {
    let id: String = row.get(at).map_err(db)?;
    let types: String = row.get(at + 2).map_err(db)?;
    let types: Vec<String> = serde_json::from_str(&types).map_err(|e| corrupt(format_args!("entity types: {e}")))?;
    let when_value: Option<String> = row.get(at + 3).map_err(db)?;
    let when = match when_value {
        Some(value) => {
            let lo: Option<i64> = row.get(at + 4).map_err(db)?;
            let hi: Option<i64> = row.get(at + 5).map_err(db)?;
            let source: String = row.get(at + 6).map_err(db)?;
            let source = match source.as_str() {
                "claim" => WhenSource::Claim,
                "recorded" => WhenSource::Recorded,
                other => return Err(corrupt(format_args!("when source {other:?}"))),
            };
            Some(EntityWhen {
                value,
                from: lo.map(|t| Timestamp(t).format()),
                to: hi.map(|t| Timestamp(t).format()),
                source,
            })
        }
        None => None,
    };
    let lat: Option<f64> = row.get(at + 7).map_err(db)?;
    let lon: Option<f64> = row.get(at + 8).map_err(db)?;
    let where_ = match (lat, lon) {
        (Some(lat), Some(lon)) => Some(EntityWhere { lat, lon, r: row.get(at + 9).map_err(db)? }),
        (None, None) => None,
        _ => return Err(corrupt(format_args!("entity {id} has half a geo point"))),
    };
    let claims: String = row.get(at + 10).map_err(db)?;
    let claims: Vec<String> = serde_json::from_str(&claims).map_err(|e| corrupt(format_args!("entity claims: {e}")))?;
    Ok(EntityView {
        id: id_from(&id)?,
        spaces: vec![space.clone()],
        label: row.get(at + 1).map_err(db)?,
        types: types.iter().map(|t| term_from(t)).collect::<Result<_>>()?,
        when,
        where_,
        score: None,
        distance_km: None,
        claims: claims.iter().map(|c| id_from(c)).collect::<Result<_>>()?,
    })
}
