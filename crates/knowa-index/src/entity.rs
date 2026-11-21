//! Entity summaries (DESIGN §4.7 display value, §4.8 when) and per-claim full-text
//! documents.
//!
//! A summary is a function of a set of claims about one entity (`summarize`). The
//! `entities` table stores it for the default view's claim set (accepted|proposed);
//! reads under any other view compute it from the claims passing that view. Both go
//! through `summarize` and `Summary::view`, so they cannot drift apart.

use std::collections::BTreeSet;

use knowa_model::view::{EntityWhen, EntityWhere, WhenSource};
use knowa_model::{EntityView, Id, IdKind, Result, Status, Timestamp, Value, When, vocab};
use rusqlite::Connection;

use crate::blobs::{self, BlobSource};
use crate::db::{corrupt, db, id_from, status_from, term_from, value_from};
use crate::fts;

/// A claim about an entity, as display ordering and summaries need it.
pub(crate) struct Evidence {
    id: String,
    at: i64,
    accepted: bool,
    p: String,
    o: Value,
}

/// Columns read by `evidence_at`, for a claims table aliased `c`.
pub(crate) const EVIDENCE_COLS: &str = "c.id, c.p, c.o, c.at, c.status";

/// Builds `Evidence` from `EVIDENCE_COLS` at columns `at..at+5`.
pub(crate) fn evidence_at(row: &rusqlite::Row<'_>, at: usize) -> Result<Evidence> {
    let o: String = row.get(at + 2).map_err(db)?;
    let status = status_from(&row.get::<_, String>(at + 4).map_err(db)?)?;
    Ok(Evidence {
        id: row.get(at).map_err(db)?,
        p: row.get(at + 1).map_err(db)?,
        o: value_from(&o)?,
        at: row.get(at + 3).map_err(db)?,
        accepted: status == Status::Accepted,
    })
}

/// Display order (DESIGN §4.7): accepted before proposed, then latest `at`; the id
/// breaks ties so the choice is deterministic.
fn display_sort(v: &mut [Evidence]) {
    v.sort_by(|a, b| b.accepted.cmp(&a.accepted).then(b.at.cmp(&a.at)).then(b.id.cmp(&a.id)));
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WhenSummary {
    pub value: String,
    pub lo: Option<i64>,
    pub hi: Option<i64>,
    pub source: WhenSource,
    /// ordering key for `when` / `-when`
    pub key: Option<i64>,
}

/// An entity's display fields, computed from one set of claims about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Summary {
    /// earliest record time among the claims; `None` iff there are none
    pub first_at: Option<i64>,
    pub label: Option<String>,
    pub types: Vec<String>,
    pub when: Option<WhenSummary>,
    /// (lat, lon, r)
    pub point: Option<(f64, f64, Option<f64>)>,
    /// supporting claim ids, deduplicated, in field order
    pub claims: Vec<String>,
}

/// The summary of `evidence`, all claims about one entity (DESIGN §4.7 display value,
/// §4.8 when).
pub(crate) fn summarize(mut evidence: Vec<Evidence>) -> Summary {
    display_sort(&mut evidence);
    let mut supporting: Vec<&str> = Vec::new();
    // The display-chosen text value of `p`.
    let shown_text = |p: &str| {
        evidence.iter().find_map(|a| match &a.o {
            Value::Text { text, .. } if a.p == p => Some((text.clone(), a.id.as_str())),
            _ => None,
        })
    };
    // label (§4.7): knowa:label, else knowa:name, so a file shows the name it was added
    // under until someone labels it
    let label = shown_text(vocab::LABEL).or_else(|| shown_text(vocab::NAME));
    if let Some((_, id)) = &label {
        supporting.push(id);
    }
    let mut types: Vec<String> = Vec::new();
    for a in &evidence {
        if let Value::Term(t) = &a.o
            && a.p == vocab::TYPE
            && !types.iter().any(|x| x == t.as_str())
        {
            types.push(t.as_str().to_string());
            supporting.push(&a.id);
        }
    }

    // when, first match wins (§4.8): the display-chosen knowa:when; else the chosen
    // knowa:modified; else, for a knowa:Note only, the earliest record time; else none.
    // Record time is world time only for notes: other entities are recorded when an
    // actor learns of them, not when they happen.
    let is_note = types.iter().any(|t| t == vocab::T_NOTE);
    let earliest = evidence.iter().min_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
    let claimed = evidence.iter().find_map(|a| match &a.o {
        Value::When(w) if a.p == vocab::WHEN => Some((w, a.id.as_str())),
        _ => None,
    });
    // Not the display order: accepted before proposed, then the earliest range start
    // (an open start is the earliest), then the claim id.
    let modified = evidence
        .iter()
        .filter_map(|a| match &a.o {
            Value::When(w) if a.p == vocab::MODIFIED => Some((a, w)),
            _ => None,
        })
        .min_by(|(a, aw), (b, bw)| b.accepted.cmp(&a.accepted).then(aw.lo.cmp(&bw.lo)).then(a.id.cmp(&b.id)));
    let when = if let Some((w, id)) = claimed {
        supporting.push(id);
        Some(WhenSummary { value: w.raw.clone(), lo: w.lo, hi: w.hi, source: WhenSource::Claim, key: when_key(w) })
    } else if let Some((a, w)) = modified {
        supporting.push(&a.id);
        Some(WhenSummary { value: w.raw.clone(), lo: w.lo, hi: w.hi, source: WhenSource::File, key: when_key(w) })
    } else if is_note && let Some(e) = earliest {
        supporting.push(&e.id);
        Some(WhenSummary { value: Timestamp(e.at).format(), lo: Some(e.at), hi: Some(e.at + 1), source: WhenSource::Recorded, key: Some(e.at) })
    } else {
        None
    };

    let place = evidence.iter().find_map(|a| match &a.o {
        Value::Geo(g) if a.p == vocab::WHERE => Some((g, a.id.as_str())),
        _ => None,
    });
    let point = place.map(|(g, id)| {
        supporting.push(id);
        (g.lat.to_f64(), g.lon.to_f64(), g.r.as_ref().map(|r| r.to_f64()))
    });

    let mut seen = BTreeSet::new();
    supporting.retain(|id| seen.insert(*id));
    Summary {
        first_at: earliest.map(|e| e.at),
        label: label.map(|(l, _)| l),
        types,
        when,
        point,
        claims: supporting.into_iter().map(String::from).collect(),
    }
}

/// Ordering key of an EDTF range for `when` / `-when`. Open-ended ranges have no
/// midpoint; they sort by their finite end.
fn when_key(w: &When) -> Option<i64> {
    w.center().or(w.lo).or(w.hi)
}

impl Summary {
    /// The when-filter of DESIGN §6.1: half-open overlap of the entity's `when` with
    /// [lo, hi), open ends unbounded. An entity without `when` never matches.
    pub fn when_overlaps(&self, lo: Option<i64>, hi: Option<i64>) -> bool {
        let Some(w) = &self.when else { return false };
        hi.is_none_or(|hi| w.lo.is_none_or(|elo| elo < hi)) && lo.is_none_or(|lo| w.hi.is_none_or(|ehi| ehi > lo))
    }

    pub fn view(&self, id: &str, space: &Id) -> Result<EntityView> {
        Ok(EntityView {
            id: id_from(id)?,
            spaces: vec![space.clone()],
            label: self.label.clone(),
            types: self.types.iter().map(|t| term_from(t)).collect::<Result<_>>()?,
            when: self.when.as_ref().map(|w| EntityWhen {
                value: w.value.clone(),
                from: w.lo.map(|t| Timestamp(t).format()),
                to: w.hi.map(|t| Timestamp(t).format()),
                source: w.source,
            }),
            where_: self.point.map(|(lat, lon, r)| EntityWhere { lat, lon, r }),
            score: None,
            distance_km: None,
            claims: self.claims.iter().map(|c| id_from(c)).collect::<Result<_>>()?,
        })
    }
}

/// Accepted|proposed claims with subject `s` (and predicate `p` if given): the
/// default view's evidence.
fn active_evidence(conn: &Connection, s: &str, p: Option<&str>) -> Result<Vec<Evidence>> {
    const BASE: &str = "SELECT c.id, c.p, c.o, c.at, c.status FROM claims c WHERE c.s = ?1 AND c.ent = 1 AND c.status IN (?2, ?3)";
    let (accepted, proposed) = (Status::Accepted.as_str(), Status::Proposed.as_str());
    let mut stmt = match p {
        Some(_) => conn.prepare_cached(&format!("{BASE} AND c.p = ?4")),
        None => conn.prepare_cached(BASE),
    }
    .map_err(db)?;
    let mut rows = match p {
        Some(p) => stmt.query(rusqlite::params![s, accepted, proposed, p]),
        None => stmt.query(rusqlite::params![s, accepted, proposed]),
    }
    .map_err(db)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db)? {
        out.push(evidence_at(row, 0)?);
    }
    Ok(out)
}

/// Display-chosen accepted|proposed `knowa:mediaType` of `blob` (base id).
pub(crate) fn media_type(conn: &Connection, blob: &str) -> Result<Option<String>> {
    let mut active = active_evidence(conn, blob, Some(vocab::MEDIA_TYPE))?;
    display_sort(&mut active);
    Ok(active.into_iter().find_map(|a| match a.o {
        Value::Text { text, .. } => Some(text),
        _ => None,
    }))
}

fn is_text_type(media_type: &str) -> bool {
    media_type.to_ascii_lowercase().starts_with("text/")
}

/// Whether a blob is text is a property of the blob, decided by its display media
/// type under the default rules (as `Index::media_type` reports it), not by the
/// query view. The view decides which claims' text counts.
fn is_text_blob(conn: &Connection, blob: &str) -> Result<bool> {
    Ok(media_type(conn, blob)?.is_some_and(|m| is_text_type(&m)))
}

/// The bare blob whose text a claim with (s, p, o) carries if that blob is a text blob
/// (DESIGN §7.2, §4.7): the blob a `knowa:content` claim links, or the blob a
/// `knowa:mediaType` claim declares to be `text/*`.
pub(crate) fn linked_blob(s: &Id, p: &str, o: &Value) -> Option<Id> {
    match o {
        Value::Ref(r) if p == vocab::CONTENT && r.kind() == IdKind::Blob => Some(r.base()),
        Value::Text { text, .. } if p == vocab::MEDIA_TYPE && s.kind() == IdKind::Blob && s.fragment().is_none() && is_text_type(text) => {
            Some(s.clone())
        }
        _ => None,
    }
}

/// The text a claim contributes on its own: a text value, or a `knowa:type` name.
pub(crate) fn own_text(p: &str, o: &Value) -> Option<String> {
    match o {
        Value::Text { text, .. } => Some(text.clone()),
        Value::Term(t) if p == vocab::TYPE => Some(t.name().to_string()),
        _ => None,
    }
}

/// Replaces claim `n`'s full-text document with `parts` (none → no document).
fn write_text(conn: &Connection, n: i64, parts: &[&str], replace: bool) -> Result<()> {
    if replace {
        conn.prepare_cached("DELETE FROM claim_fts WHERE rowid = ?1").map_err(db)?.execute([n]).map_err(db)?;
    }
    if !parts.is_empty() {
        conn.prepare_cached("INSERT INTO claim_fts(rowid, body) VALUES (?1, ?2)")
            .map_err(db)?
            .execute(rusqlite::params![n, fts::document(&parts.join("\n"))])
            .map_err(db)?;
    }
    Ok(())
}

/// Indexes a newly inserted entity-materializing claim that links no blob.
pub(crate) fn insert_text(conn: &Connection, n: i64, p: &str, o: &Value) -> Result<()> {
    match own_text(p, o) {
        Some(t) => write_text(conn, n, &[&t], false),
        None => Ok(()),
    }
}

/// Rewrites the documents of every claim that links `blob` (its text/* mediaType
/// claims and the content claims pointing at it), whose text depends on whether
/// `blob` is a text blob and present.
pub(crate) fn refresh_blob_text(conn: &Connection, blobs: &dyn BlobSource, blob: &str) -> Result<()> {
    let id = id_from(blob)?;
    let text = if is_text_blob(conn, blob)? { blobs::fts_text(blobs, &id)? } else { None };
    let mut linking = Vec::new();
    for (sql, p) in [
        ("SELECT n, s, p, o FROM claims WHERE s = ?1 AND p = ?2 AND ent = 1", vocab::MEDIA_TYPE),
        ("SELECT n, s, p, o FROM claims WHERE o_ref_base = ?1 AND p = ?2 AND ent = 1", vocab::CONTENT),
    ] {
        let mut stmt = conn.prepare_cached(sql).map_err(db)?;
        let mut rows = stmt.query(rusqlite::params![blob, p]).map_err(db)?;
        while let Some(row) = rows.next().map_err(db)? {
            let n: i64 = row.get(0).map_err(db)?;
            let s = id_from(&row.get::<_, String>(1).map_err(db)?)?;
            let p: String = row.get(2).map_err(db)?;
            let o = value_from(&row.get::<_, String>(3).map_err(db)?)?;
            if linked_blob(&s, &p, &o).as_ref() == Some(&id) {
                linking.push((n, own_text(&p, &o)));
            }
        }
    }
    for (n, own) in linking {
        let parts: Vec<&str> = own.as_deref().into_iter().chain(text.as_deref()).collect();
        write_text(conn, n, &parts, true)?;
    }
    Ok(())
}

/// Columns read by `stored_at`, for an `entities` table aliased `e`.
pub(crate) const COLS: &str =
    "e.id, e.first_at, e.label, e.types, e.when_value, e.when_lo, e.when_hi, e.when_source, e.when_key, e.lat, e.lon, e.r, e.claims";

/// Recomputes the stored (default-view) summary and geo point of `entity`.
pub(crate) fn refresh(conn: &Connection, entity: &str) -> Result<()> {
    let s = summarize(active_evidence(conn, entity, None)?);
    let when = s.when.as_ref();
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
                s.first_at,
                s.label,
                serde_json::json!(s.types).to_string(),
                when.map(|w| w.value.as_str()),
                when.and_then(|w| w.lo),
                when.and_then(|w| w.hi),
                when.map(|w| source_str(w.source)),
                when.and_then(|w| w.key),
                s.point.map(|p| p.0),
                s.point.map(|p| p.1),
                s.point.and_then(|p| p.2),
                serde_json::json!(s.claims).to_string(),
            ],
            |r| r.get(0),
        )
        .map_err(db)?;

    conn.prepare_cached("DELETE FROM geo WHERE n = ?1").map_err(db)?.execute([n]).map_err(db)?;
    if let Some((lat, lon, _)) = s.point {
        conn.prepare_cached("INSERT INTO geo(n, min_lat, max_lat, min_lon, max_lon) VALUES (?1, ?2, ?2, ?3, ?3)")
            .map_err(db)?
            .execute(rusqlite::params![n, lat, lon])
            .map_err(db)?;
    }
    Ok(())
}

fn source_str(s: WhenSource) -> &'static str {
    match s {
        WhenSource::Claim => "claim",
        WhenSource::File => "file",
        WhenSource::Recorded => "recorded",
    }
}

/// Reads (id, stored summary) from `COLS` at columns `at..at+NCOLS`.
pub(crate) fn stored_at(row: &rusqlite::Row<'_>, at: usize) -> Result<(String, Summary)> {
    let id: String = row.get(at).map_err(db)?;
    let types: String = row.get(at + 3).map_err(db)?;
    let types: Vec<String> = serde_json::from_str(&types).map_err(|e| corrupt(format_args!("entity types: {e}")))?;
    let when_value: Option<String> = row.get(at + 4).map_err(db)?;
    let when = match when_value {
        Some(value) => {
            let source: String = row.get(at + 7).map_err(db)?;
            let source = match source.as_str() {
                "claim" => WhenSource::Claim,
                "file" => WhenSource::File,
                "recorded" => WhenSource::Recorded,
                other => return Err(corrupt(format_args!("when source {other:?}"))),
            };
            Some(WhenSummary {
                value,
                lo: row.get(at + 5).map_err(db)?,
                hi: row.get(at + 6).map_err(db)?,
                source,
                key: row.get(at + 8).map_err(db)?,
            })
        }
        None => None,
    };
    let lat: Option<f64> = row.get(at + 9).map_err(db)?;
    let lon: Option<f64> = row.get(at + 10).map_err(db)?;
    let point = match (lat, lon) {
        (Some(lat), Some(lon)) => Some((lat, lon, row.get(at + 11).map_err(db)?)),
        (None, None) => None,
        _ => return Err(corrupt(format_args!("entity {id} has half a geo point"))),
    };
    let claims: String = row.get(at + 12).map_err(db)?;
    let claims: Vec<String> = serde_json::from_str(&claims).map_err(|e| corrupt(format_args!("entity claims: {e}")))?;
    let summary = Summary { first_at: row.get(at + 1).map_err(db)?, label: row.get(at + 2).map_err(db)?, types, when, point, claims };
    Ok((id, summary))
}
