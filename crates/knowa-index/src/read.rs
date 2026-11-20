//! Single-replica reads: find (claims/entities), lookups, log, feed, actors.

use std::collections::{BTreeMap, BTreeSet};

use knowa_model::query::{Near, Order, View, Where};
use knowa_model::{ActorInfo, ClaimView, Dec, Dir, Edge, EntityView, Error, Id, Result, Timestamp, When};
use rusqlite::{Connection, params_from_iter};

use crate::Page;
use crate::db::{Sql, canonical, check_limit, claim_view_at, conf_key, db, i64_of, id_from, make_cursor, parse_cursor, term_from, u64_of};
use crate::entity;
use crate::fts;
use crate::geo;
use crate::policy;

/// A validated `View`, rendered as conditions on a claims alias.
pub(crate) struct ViewF {
    statuses: Vec<String>,
    kinds: Vec<String>,
    actors: Vec<String>,
    include_stale: bool,
    min_conf: Option<String>,
}

impl ViewF {
    pub fn parse(v: &View) -> Result<ViewF> {
        let min_conf = match &v.min_conf {
            Some(m) => {
                Dec::parse(m)?;
                Some(conf_key(m)?)
            }
            None => None,
        };
        Ok(ViewF {
            statuses: v.status.iter().map(|s| s.as_str().to_string()).collect(),
            kinds: v.actor_kinds.iter().map(|k| k.as_term().to_string()).collect(),
            actors: v.actors.iter().map(|a| a.as_str().to_string()).collect(),
            include_stale: v.include_stale,
            min_conf,
        })
    }

    /// Appends ` AND …` conditions on claims aliased `c`. Actor kinds are the
    /// self-declared ones: an actor without a declared kind matches no kind filter.
    pub fn sql(&self, q: &mut Sql, c: &str) {
        if self.statuses.is_empty() {
            q.add(" AND 0");
        } else {
            q.add(&format!(" AND {c}.status IN ")).list(self.statuses.iter().cloned());
        }
        if !self.kinds.is_empty() {
            q.add(&format!(" AND {c}.actor IN (SELECT id FROM actors WHERE kind IN "))
                .list(self.kinds.iter().cloned())
                .add(")");
        }
        if !self.actors.is_empty() {
            q.add(&format!(" AND {c}.actor IN ")).list(self.actors.iter().cloned());
        }
        if !self.include_stale {
            q.add(&format!(" AND {c}.stale = 0"));
        }
        if let Some(m) = &self.min_conf {
            q.add(&format!(" AND {c}.conf >= ")).arg(m.clone());
        }
    }
}

struct NearF {
    lat: f64,
    lon: f64,
    km: f64,
}

impl NearF {
    fn parse(n: &Near) -> Result<NearF> {
        let num = |name: &str, s: &str, lo: f64, hi: f64| -> Result<f64> {
            let v = Dec::parse(s)?.to_f64();
            if !(lo..=hi).contains(&v) {
                return Err(Error::invalid(format!("near.{name} {s} is outside [{lo}, {hi}]")));
            }
            Ok(v)
        };
        Ok(NearF {
            lat: num("lat", &n.lat, -90.0, 90.0)?,
            lon: num("lon", &n.lon, -180.0, 180.0)?,
            km: num("km", &n.km, 0.0, f64::MAX)?,
        })
    }
}

/// A validated `Where`.
pub(crate) struct WhereF {
    ids: Vec<String>,
    s: Vec<String>,
    p: Vec<String>,
    o: Option<String>,
    types: Vec<String>,
    /// FTS5 MATCH expression
    text: Option<String>,
    /// [lo, hi) with None = open
    when: Option<(Option<i64>, Option<i64>)>,
    near: Option<NearF>,
    actors: Vec<String>,
    after: Option<i64>,
    before: Option<i64>,
}

impl WhereF {
    pub fn parse(w: &Where) -> Result<WhereF> {
        if w.near_id.is_some() {
            return Err(Error::invalid("where.nearId must be resolved to where.near before it reaches an index"));
        }
        let when = match &w.when {
            Some(r) => {
                let lo = match &r.from {
                    Some(f) => When::parse(f)?.lo,
                    None => None,
                };
                let hi = match &r.to {
                    Some(t) => When::parse(t)?.hi,
                    None => None,
                };
                Some((lo, hi))
            }
            None => None,
        };
        let strs = |v: &[Id]| v.iter().map(|x| x.as_str().to_string()).collect::<Vec<_>>();
        Ok(WhereF {
            ids: strs(&w.ids),
            s: strs(&w.s),
            p: w.p.iter().map(|t| t.as_str().to_string()).collect(),
            o: w.o.as_ref().map(canonical).transpose()?,
            types: w.type_.iter().map(|t| t.as_str().to_string()).collect(),
            text: w.text.as_deref().map(fts::match_expr).transpose()?,
            when,
            near: w.near.as_ref().map(NearF::parse).transpose()?,
            actors: strs(&w.actors),
            after: w.recorded_after.as_deref().map(Timestamp::parse_rfc3339).transpose()?.map(|t| t.0),
            before: w.recorded_before.as_deref().map(Timestamp::parse_rfc3339).transpose()?.map(|t| t.0),
        })
    }

    /// Claim-level filters on claims aliased `c` (`recordedAfter` inclusive,
    /// `recordedBefore` exclusive).
    fn claim_sql(&self, q: &mut Sql, c: &str) {
        if !self.s.is_empty() {
            q.add(&format!(" AND {c}.s IN ")).list(self.s.iter().cloned());
        }
        if !self.p.is_empty() {
            q.add(&format!(" AND {c}.p IN ")).list(self.p.iter().cloned());
        }
        if let Some(o) = &self.o {
            q.add(&format!(" AND {c}.o = ")).arg(o.clone());
        }
        if !self.actors.is_empty() {
            q.add(&format!(" AND {c}.actor IN ")).list(self.actors.iter().cloned());
        }
        if let Some(a) = self.after {
            q.add(&format!(" AND {c}.at >= ")).arg(a);
        }
        if let Some(b) = self.before {
            q.add(&format!(" AND {c}.at < ")).arg(b);
        }
    }

    /// Entity-level filters on entities aliased `e` (type, when, near prefilter).
    fn entity_sql(&self, q: &mut Sql, e: &str) {
        if !self.types.is_empty() {
            q.add(&format!(" AND EXISTS (SELECT 1 FROM entity_types t WHERE t.entity = {e}.n AND t.type IN "))
                .list(self.types.iter().cloned())
                .add(")");
        }
        if let Some((lo, hi)) = self.when {
            // half-open overlap: [elo, ehi) ∩ [lo, hi) ≠ ∅, open ends unbounded
            q.add(&format!(" AND {e}.when_value IS NOT NULL"));
            if let Some(hi) = hi {
                q.add(&format!(" AND ({e}.when_lo IS NULL OR {e}.when_lo < ")).arg(hi).add(")");
            }
            if let Some(lo) = lo {
                q.add(&format!(" AND ({e}.when_hi IS NULL OR {e}.when_hi > ")).arg(lo).add(")");
            }
        }
        if let Some(n) = &self.near {
            let b = geo::bbox(n.lat, n.lon, n.km);
            q.add(&format!(" AND {e}.n IN (SELECT n FROM geo WHERE max_lat >= "))
                .arg(b.min_lat)
                .add(" AND min_lat <= ")
                .arg(b.max_lat)
                .add(" AND max_lon >= ")
                .arg(b.min_lon)
                .add(" AND min_lon <= ")
                .arg(b.max_lon)
                .add(")");
        }
    }
}

#[derive(Clone, Copy)]
enum Sort {
    Score,
    When { desc: bool },
    Recorded { desc: bool },
    Distance,
}

fn sort_of(order: Order, w: &WhereF, entities: bool) -> Result<Sort> {
    Ok(match order {
        Order::Relevance if w.text.is_some() => Sort::Score,
        Order::Relevance if entities => Sort::When { desc: true },
        Order::Relevance => Sort::Recorded { desc: true },
        Order::When => Sort::When { desc: false },
        Order::WhenDesc => Sort::When { desc: true },
        Order::Recorded => Sort::Recorded { desc: false },
        Order::RecordedDesc => Sort::Recorded { desc: true },
        Order::Distance if w.near.is_some() => Sort::Distance,
        Order::Distance => return Err(Error::invalid("order \"distance\" requires where.near")),
    })
}

fn rows<T>(conn: &Connection, q: &Sql, mut f: impl FnMut(&rusqlite::Row<'_>) -> Result<T>) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(&q.text).map_err(db)?;
    let mut rows = stmt.query(params_from_iter(q.params.iter())).map_err(db)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db)? {
        out.push(f(row)?);
    }
    Ok(out)
}

/// `fetched` holds up to `limit + 1` items starting at `offset`.
fn page<T>(mut fetched: Vec<T>, offset: u64, limit: i64) -> Result<Page<T>> {
    let limit = usize::try_from(limit).map_err(|_| Error::internal("limit out of range"))?;
    let more = fetched.len() > limit;
    fetched.truncate(limit);
    let next = offset + fetched.len() as u64;
    Ok(Page { items: fetched, cursor: more.then(|| make_cursor(next)) })
}

/// Exact distance filter and (for `Sort::Distance`) ordering, then the page slice.
/// Rows arrive in SQL order; the sort is stable so ties keep it.
fn near_page<T>(
    mut items: Vec<(T, Option<(f64, f64)>)>,
    near: &NearF,
    sort: Sort,
    offset: u64,
    limit: i64,
    mut set_distance: impl FnMut(&mut T, f64),
) -> Result<Page<T>> {
    let mut kept: Vec<(T, f64)> = Vec::new();
    for (mut item, point) in items.drain(..) {
        let Some((lat, lon)) = point else { continue };
        let d = geo::haversine_km(near.lat, near.lon, lat, lon);
        if d <= near.km {
            set_distance(&mut item, d);
            kept.push((item, d));
        }
    }
    if matches!(sort, Sort::Distance) {
        kept.sort_by(|a, b| a.1.total_cmp(&b.1));
    }
    let skip = usize::try_from(offset).map_err(|_| Error::invalid("cursor offset out of range"))?;
    let take = usize::try_from(limit + 1).map_err(|_| Error::internal("limit out of range"))?;
    let fetched = kept.into_iter().skip(skip).take(take).map(|(t, _)| t).collect();
    page(fetched, offset, limit)
}

pub(crate) fn find_entities(
    conn: &Connection,
    space: &Id,
    w: &Where,
    view: &View,
    order: Order,
    limit: u32,
    cursor: Option<&str>,
) -> Result<Page<EntityView>> {
    let w = WhereF::parse(w)?;
    let v = ViewF::parse(view)?;
    let limit = check_limit(limit)?;
    let offset = parse_cursor(cursor)?;
    let sort = sort_of(order, &w, true)?;

    let mut q = Sql::new("SELECT ");
    q.add(entity::COLS);
    match &w.text {
        Some(m) => {
            q.add(", -bm25(fts) AS score FROM fts JOIN entities e ON e.n = fts.rowid WHERE fts MATCH ").arg(m.clone());
        }
        None => {
            q.add(", NULL AS score FROM entities e WHERE 1");
        }
    }
    if !w.ids.is_empty() {
        q.add(" AND e.id IN ").list(w.ids.iter().cloned());
    }
    w.entity_sql(&mut q, "e");
    q.add(" AND EXISTS (SELECT 1 FROM claims c WHERE c.s = e.id AND c.ent = 1");
    v.sql(&mut q, "c");
    w.claim_sql(&mut q, "c");
    q.add(")");
    q.add(match sort {
        Sort::Score => " ORDER BY score DESC, e.id",
        Sort::When { desc: false } => " ORDER BY e.when_key IS NULL, e.when_key, e.id",
        Sort::When { desc: true } => " ORDER BY e.when_key IS NULL, e.when_key DESC, e.id",
        Sort::Recorded { desc: false } => " ORDER BY e.first_at, e.id",
        Sort::Recorded { desc: true } => " ORDER BY e.first_at DESC, e.id",
        Sort::Distance => " ORDER BY e.id",
    });
    let read = |row: &rusqlite::Row<'_>| -> Result<EntityView> {
        let mut e = entity::view_at(row, 0, space)?;
        e.score = row.get(entity::NCOLS).map_err(db)?;
        Ok(e)
    };
    match &w.near {
        None => {
            q.add(" LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
            page(rows(conn, &q, read)?, offset, limit)
        }
        Some(n) => {
            let all = rows(conn, &q, read)?
                .into_iter()
                .map(|e| {
                    let point = e.where_.as_ref().map(|p| (p.lat, p.lon));
                    (e, point)
                })
                .collect();
            near_page(all, n, sort, offset, limit, |e, d| e.distance_km = Some(d))
        }
    }
}

pub(crate) fn find_claims(
    conn: &Connection,
    w: &Where,
    view: &View,
    order: Order,
    limit: u32,
    cursor: Option<&str>,
) -> Result<Page<ClaimView>> {
    let w = WhereF::parse(w)?;
    let v = ViewF::parse(view)?;
    let limit = check_limit(limit)?;
    let offset = parse_cursor(cursor)?;
    let sort = sort_of(order, &w, false)?;

    // Entity-level filters (type, text, when, near) apply to the claim's subject entity.
    let mut q = Sql::new("SELECT c.json, c.status, c.stale, e.lat, e.lon");
    match &w.text {
        Some(m) => {
            q.add(", -bm25(fts) AS score FROM fts JOIN entities e ON e.n = fts.rowid JOIN claims c ON c.s = e.id WHERE fts MATCH ")
                .arg(m.clone());
        }
        None => {
            q.add(", NULL AS score FROM claims c LEFT JOIN entities e ON e.id = c.s WHERE 1");
        }
    }
    if !w.ids.is_empty() {
        q.add(" AND c.id IN ").list(w.ids.iter().cloned());
    }
    v.sql(&mut q, "c");
    w.claim_sql(&mut q, "c");
    w.entity_sql(&mut q, "e");
    q.add(match sort {
        Sort::Score => " ORDER BY score DESC, c.id",
        Sort::When { desc: false } => " ORDER BY e.when_key IS NULL, e.when_key, c.id",
        Sort::When { desc: true } => " ORDER BY e.when_key IS NULL, e.when_key DESC, c.id",
        Sort::Recorded { desc: false } => " ORDER BY c.at, c.id",
        Sort::Recorded { desc: true } => " ORDER BY c.at DESC, c.id",
        Sort::Distance => " ORDER BY c.id",
    });
    match &w.near {
        None => {
            q.add(" LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
            page(rows(conn, &q, |row| claim_view_at(row, 0))?, offset, limit)
        }
        Some(n) => {
            let all = rows(conn, &q, |row| {
                let lat: Option<f64> = row.get(3).map_err(db)?;
                let lon: Option<f64> = row.get(4).map_err(db)?;
                Ok((claim_view_at(row, 0)?, lat.zip(lon)))
            })?;
            near_page(all, n, sort, offset, limit, |_, _| {})
        }
    }
}

pub(crate) fn entity(conn: &Connection, space: &Id, id: &Id, view: &View) -> Result<Option<EntityView>> {
    let v = ViewF::parse(view)?;
    let mut q = Sql::new("SELECT ");
    q.add(entity::COLS).add(" FROM entities e WHERE e.id = ").arg(id.as_str().to_string());
    q.add(" AND EXISTS (SELECT 1 FROM claims c WHERE c.s = e.id AND c.ent = 1");
    v.sql(&mut q, "c");
    q.add(")");
    Ok(rows(conn, &q, |row| entity::view_at(row, 0, space))?.into_iter().next())
}

pub(crate) fn claims(conn: &Connection, ids: &[Id]) -> Result<Vec<ClaimView>> {
    let mut stmt = conn.prepare_cached("SELECT json, status, stale FROM claims WHERE id = ?1").map_err(db)?;
    let mut out = Vec::new();
    for id in ids {
        let mut rows = stmt.query([id.as_str()]).map_err(db)?;
        if let Some(row) = rows.next().map_err(db)? {
            out.push(claim_view_at(row, 0)?);
        }
    }
    Ok(out)
}

pub(crate) fn equivalent(conn: &Connection, actor: &Id, equiv_key: &str) -> Result<Option<ClaimView>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT json, status, stale FROM claims WHERE equiv = ?1 AND actor = ?2 AND status != ?3 ORDER BY seq LIMIT 1",
        )
        .map_err(db)?;
    let mut rows = stmt.query([equiv_key, actor.as_str(), knowa_model::Status::Retracted.as_str()]).map_err(db)?;
    match rows.next().map_err(db)? {
        Some(row) => Ok(Some(claim_view_at(row, 0)?)),
        None => Ok(None),
    }
}

pub(crate) fn claims_about(conn: &Connection, id: &Id, view: &View, limit: u32) -> Result<(Vec<ClaimView>, Vec<ClaimView>)> {
    let v = ViewF::parse(view)?;
    let limit = check_limit(limit)?;
    let list = |column: &str, value: &str| -> Result<Vec<ClaimView>> {
        let mut q = Sql::new("SELECT c.json, c.status, c.stale FROM claims c WHERE ");
        q.add(&format!("c.{column} = ")).arg(value.to_string());
        v.sql(&mut q, "c");
        q.add(" ORDER BY c.at, c.id LIMIT ").arg(limit);
        rows(conn, &q, |row| claim_view_at(row, 0))
    };
    Ok((list("s", id.as_str())?, list("o_ref_base", id.base().as_str())?))
}

pub(crate) fn edges(conn: &Connection, id: &Id, view: &View) -> Result<Vec<Edge>> {
    let v = ViewF::parse(view)?;
    let mut out = Vec::new();
    // (direction, matched column, value, column holding the other end)
    let sides = [(Dir::Out, "s", id.as_str().to_string(), "json_extract(c.o, '$.ref')"), (Dir::In, "o_ref_base", id.base().as_str().to_string(), "c.s")];
    for (dir, column, value, other) in sides {
        let mut q = Sql::new(&format!("SELECT c.id, c.p, {other} FROM claims c WHERE c.policy = 0 AND c.o_ref_base IS NOT NULL AND c.{column} = "));
        q.arg(value);
        v.sql(&mut q, "c");
        q.add(" ORDER BY c.at, c.id");
        out.extend(rows(conn, &q, |row| {
            Ok(Edge {
                from: id.clone(),
                to: id_from(&row.get::<_, String>(2).map_err(db)?)?,
                p: term_from(&row.get::<_, String>(1).map_err(db)?)?,
                via: id_from(&row.get::<_, String>(0).map_err(db)?)?,
                dir,
            })
        })?);
    }
    Ok(out)
}

pub(crate) fn feed(conn: &Connection, after: u64, limit: u32) -> Result<(Vec<ClaimView>, u64)> {
    let limit = check_limit(limit)?;
    let mut stmt = conn
        .prepare_cached("SELECT n, json, status, stale FROM claims WHERE n > ?1 ORDER BY n LIMIT ?2")
        .map_err(db)?;
    let mut rows = stmt.query([i64_of(after, "feed position")?, limit]).map_err(db)?;
    let mut out = Vec::new();
    let mut last = after;
    while let Some(row) = rows.next().map_err(db)? {
        last = u64_of(row.get(0).map_err(db)?, "arrival number")?;
        out.push(claim_view_at(row, 1)?);
    }
    Ok((out, last))
}

pub(crate) fn log(conn: &Connection, actors: &[Id], since_ms: Option<i64>, limit: u32, cursor: Option<&str>) -> Result<Page<ClaimView>> {
    let limit = check_limit(limit)?;
    let offset = parse_cursor(cursor)?;
    let mut q = Sql::new("SELECT c.json, c.status, c.stale FROM claims c WHERE 1");
    if !actors.is_empty() {
        q.add(" AND c.actor IN ").list(actors.iter().map(|a| a.as_str().to_string()));
    }
    if let Some(since) = since_ms {
        q.add(" AND c.at >= ").arg(since);
    }
    q.add(" ORDER BY c.at, c.id LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
    page(rows(conn, &q, |row| claim_view_at(row, 0))?, offset, limit)
}

pub(crate) fn actor_heads(conn: &Connection) -> Result<Vec<(Id, u64)>> {
    let q = Sql::new("SELECT actor, MAX(seq) FROM claims GROUP BY actor ORDER BY actor");
    rows(conn, &q, |row| Ok((id_from(&row.get::<_, String>(0).map_err(db)?)?, u64_of(row.get(1).map_err(db)?, "seq")?)))
}

pub(crate) fn actors(conn: &Connection) -> Result<Vec<ActorInfo>> {
    let q = Sql::new("SELECT actor, COUNT(*) FROM claims GROUP BY actor");
    let counts: BTreeMap<String, u64> =
        rows(conn, &q, |row| Ok((row.get::<_, String>(0).map_err(db)?, u64_of(row.get(1).map_err(db)?, "count")?)))?
            .into_iter()
            .collect();
    let state = policy::load(conn)?;
    let ids: BTreeSet<&String> = counts.keys().chain(state.iter().filter(|(_, p)| p.role.is_some()).map(|(id, _)| id)).collect();
    ids.into_iter()
        .map(|id| {
            let p = state.get(id);
            Ok(ActorInfo {
                id: id_from(id)?,
                kind: p.and_then(|p| p.kind),
                role: p.and_then(|p| p.role),
                auto_accept: p.is_some_and(|p| p.auto),
                claims: counts.get(id).copied().unwrap_or(0),
            })
        })
        .collect()
}

pub(crate) fn media_type(conn: &Connection, blob: &Id) -> Result<Option<String>> {
    entity::media_type(conn, blob.base().as_str())
}

/// Base ids of subjects of claims written by `worker`.
pub(crate) fn worker_subjects(conn: &Connection, worker: &str) -> Result<BTreeSet<Id>> {
    let mut q = Sql::new("SELECT DISTINCT s FROM claims WHERE worker = ");
    q.arg(worker.to_string());
    Ok(rows(conn, &q, |row| Ok(id_from(&row.get::<_, String>(0).map_err(db)?)?.base()))?.into_iter().collect())
}
