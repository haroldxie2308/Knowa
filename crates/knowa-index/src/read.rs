//! Single-replica reads: find (claims/entities), lookups, log, feed, actors.
//!
//! Views apply to evidence (DESIGN §4.7): an entity matches `text`, `type`, `when` and
//! `near`, and gets its display fields, only through claims that pass the view. Two
//! routes implement that, chosen by the view alone:
//! - stored: the default view reads the summaries kept in `entities` and pages in SQL;
//! - computed: any other view summarizes each candidate entity from its passing
//!   claims with `entity::summarize`, the function that also fills `entities`.
//!
//! Text matching and scoring (`text_scores`), type matching and the result order
//! (`Keys`, mirroring the SQL ORDER BY) are shared by both routes.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use knowa_model::query::{Near, Order, View, Where};
use knowa_model::{ActorInfo, ClaimView, Dec, Dir, Edge, EntityView, Error, Id, Result, Status, Timestamp, Value, When, vocab};
use rusqlite::{Connection, params_from_iter};

use crate::Page;
use crate::db::{Sql, canonical, check_limit, claim_view_at, conf_key, db, i64_of, id_from, make_cursor, parse_cursor, term_from, u64_of};
use crate::entity::{self, Summary};
use crate::fts;
use crate::geo;
use crate::policy;

/// `knowa:type` as an SQL literal: the `claims_type` partial index is used only when
/// a query spells out its condition literally.
const TYPE_LITERAL: &str = "'knowa:type'";
const _: () = assert!(matches!(vocab::TYPE.as_bytes(), b"knowa:type"));

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

    /// The default read view (DESIGN §4.7): exactly the claim set whose summaries
    /// `entities` stores. Recognized syntactically: a view that passes the same claims
    /// another way (e.g. `minConf: "0"`) takes the computed route, which is how tests
    /// check both routes against each other.
    fn is_default(&self) -> bool {
        let statuses: BTreeSet<&str> = self.statuses.iter().map(String::as_str).collect();
        statuses == BTreeSet::from([Status::Accepted.as_str(), Status::Proposed.as_str()])
            && self.kinds.is_empty()
            && self.actors.is_empty()
            && self.include_stale
            && self.min_conf.is_none()
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

    /// The distance of `point` if it lies within the circle; `None` if it does not
    /// (or there is no point).
    fn within(&self, point: Option<(f64, f64)>) -> Option<f64> {
        let (lat, lon) = point?;
        let d = geo::haversine_km(self.lat, self.lon, lat, lon);
        (d <= self.km).then_some(d)
    }
}

/// A validated `Where`.
pub(crate) struct WhereF {
    ids: Vec<String>,
    s: Vec<String>,
    p: Vec<String>,
    o: Option<String>,
    /// canonical JSON of the `{"term": …}` values
    types: Vec<String>,
    /// one FTS5 MATCH expression per query token
    terms: Option<Vec<String>>,
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
                let from = r.from.as_deref().map(When::parse).transpose()?;
                let to = r.to.as_deref().map(When::parse).transpose()?;
                let lo = from.as_ref().and_then(|f| f.lo);
                let hi = to.as_ref().and_then(|t| t.hi);
                if let (Some(f), Some(t)) = (&from, &to)
                    && let (Some(lo), Some(hi)) = (lo, hi)
                    && lo >= hi
                {
                    return Err(Error::invalid(format!(
                        "where.when is reversed: from {:?} starts at or after the end of to {:?}",
                        f.raw, t.raw
                    )));
                }
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
            types: w.type_.iter().map(|t| canonical(&Value::Term(t.clone()))).collect::<Result<_>>()?,
            terms: w.text.as_deref().map(fts::match_terms).transpose()?,
            when,
            near: w.near.as_ref().map(NearF::parse).transpose()?,
            actors: strs(&w.actors),
            after: w.recorded_after.as_deref().map(Timestamp::parse_rfc3339).transpose()?.map(|t| t.0),
            before: w.recorded_before.as_deref().map(Timestamp::parse_rfc3339).transpose()?.map(|t| t.0),
        })
    }

    fn has_claim_filters(&self) -> bool {
        !self.s.is_empty()
            || !self.p.is_empty()
            || self.o.is_some()
            || !self.actors.is_empty()
            || self.after.is_some()
            || self.before.is_some()
    }

    fn has_entity_filters(&self) -> bool {
        !self.types.is_empty() || self.terms.is_some() || self.when.is_some() || self.near.is_some()
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

    /// `SELECT t.s FROM claims t WHERE …`: subjects of `knowa:type` claims passing `v`
    /// with one of the wanted types. Callers check `types` is non-empty.
    fn type_subjects_sql(&self, q: &mut Sql, v: &ViewF) {
        q.add(&format!("SELECT t.s FROM claims t WHERE t.p = {TYPE_LITERAL} AND t.ent = 1 AND t.o IN "))
            .list(self.types.iter().cloned());
        v.sql(q, "t");
    }

    /// Entity-level filters on the stored (default-view) summaries of `entities`
    /// aliased `e`: type, when, and the near prefilter (the exact distance check
    /// follows in Rust).
    fn stored_sql(&self, q: &mut Sql, v: &ViewF) {
        if !self.types.is_empty() {
            q.add(" AND e.id IN (");
            self.type_subjects_sql(q, v);
            q.add(")");
        }
        if let Some((lo, hi)) = self.when {
            // half-open overlap, as `Summary::when_overlaps`
            q.add(" AND e.when_value IS NOT NULL");
            if let Some(hi) = hi {
                q.add(" AND (e.when_lo IS NULL OR e.when_lo < ").arg(hi).add(")");
            }
            if let Some(lo) = lo {
                q.add(" AND (e.when_hi IS NULL OR e.when_hi > ").arg(lo).add(")");
            }
        }
        if let Some(n) = &self.near {
            let b = geo::bbox(n.lat, n.lon, n.km);
            q.add(" AND e.n IN (SELECT n FROM geo WHERE max_lat >= ")
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
        Order::Relevance if w.terms.is_some() => Sort::Score,
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

/// What a result is ordered by. `order` is the total order both routes use; the SQL
/// ORDER BY of the stored route (`sql_order`) is the same order.
struct Keys<'a> {
    /// entity id, or claim id for claims
    id: &'a str,
    score: Option<f64>,
    when: Option<i64>,
    recorded: Option<i64>,
    distance: Option<f64>,
}

/// Present before absent. Never compares an absent value in practice: every hit of a
/// text query has a score and every hit of a near query a distance.
fn cmp_f64(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

impl Keys<'_> {
    fn order(&self, b: &Keys<'_>, sort: Sort) -> Ordering {
        let a = self;
        let primary = match sort {
            Sort::Score => cmp_f64(b.score, a.score),
            // `when IS NULL, when [DESC]`
            Sort::When { desc } => a.when.is_none().cmp(&b.when.is_none()).then(if desc { b.when.cmp(&a.when) } else { a.when.cmp(&b.when) }),
            // SQL sorts NULL first ascending and last descending, as Option does
            Sort::Recorded { desc } => {
                if desc {
                    b.recorded.cmp(&a.recorded)
                } else {
                    a.recorded.cmp(&b.recorded)
                }
            }
            Sort::Distance => cmp_f64(a.distance, b.distance),
        };
        primary.then(a.id.cmp(b.id))
    }
}

fn recorded_order(desc: bool, recorded: &str, id: &str) -> String {
    if desc { format!(" ORDER BY {recorded} DESC, {id}") } else { format!(" ORDER BY {recorded}, {id}") }
}

/// `Keys::order` as SQL over `when`, `recorded` and `id` columns, for the sorts SQL
/// can do (score and distance are computed in Rust).
fn sql_order(sort: Sort, when: &str, recorded: &str, id: &str) -> Option<String> {
    match sort {
        Sort::When { desc: false } => Some(format!(" ORDER BY {when} IS NULL, {when}, {id}")),
        Sort::When { desc: true } => Some(format!(" ORDER BY {when} IS NULL, {when} DESC, {id}")),
        Sort::Recorded { desc } => Some(recorded_order(desc, recorded, id)),
        Sort::Score | Sort::Distance => None,
    }
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

/// Sorts every result, then takes the page at `offset`.
fn sorted_page<T>(mut items: Vec<T>, cmp: impl Fn(&T, &T) -> Ordering, offset: u64, limit: i64) -> Result<Page<T>> {
    items.sort_by(cmp);
    let skip = usize::try_from(offset).map_err(|_| Error::invalid("cursor offset out of range"))?;
    let take = usize::try_from(limit + 1).map_err(|_| Error::internal("limit out of range"))?;
    page(items.into_iter().skip(skip).take(take).collect(), offset, limit)
}

fn json_list<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    serde_json::json!(items.into_iter().collect::<Vec<_>>()).to_string()
}

/// Entities whose passing claims contain every token of a text query, with their
/// relevance (higher = better).
///
/// Each claim is its own FTS document, so bm25 runs once per token. An entity's
/// score for a token is the best `-bm25` among its passing claims that match the
/// token, and its score is the sum of those over the tokens, in query order. That
/// mirrors bm25 itself (a sum over query terms), scoring each token on the claim that
/// is the best evidence for it; corpus statistics cover every claim document,
/// whatever the view.
fn text_scores(conn: &Connection, terms: &[String], v: &ViewF) -> Result<BTreeMap<String, f64>> {
    let mut total: Option<BTreeMap<String, f64>> = None;
    for term in terms {
        let mut q = Sql::new("SELECT c.s, -bm25(claim_fts) FROM claim_fts JOIN claims c ON c.n = claim_fts.rowid WHERE claim_fts MATCH ");
        q.arg(term.clone()).add(" AND c.ent = 1");
        v.sql(&mut q, "c");
        let mut best: BTreeMap<String, f64> = BTreeMap::new();
        for (s, score) in rows(conn, &q, |r| Ok((r.get::<_, String>(0).map_err(db)?, r.get::<_, f64>(1).map_err(db)?)))? {
            best.entry(s).and_modify(|b| *b = b.max(score)).or_insert(score);
        }
        total = Some(match total {
            None => best,
            Some(acc) => acc.into_iter().filter_map(|(s, sum)| best.get(&s).map(|b| (s, sum + b))).collect(),
        });
    }
    total.ok_or_else(|| Error::internal("a text query without tokens reached the index"))
}

/// Summaries under `v` of the entities that have a passing claim, restricted to
/// `only` when given.
fn computed_summaries(conn: &Connection, v: &ViewF, only: Option<&BTreeSet<String>>) -> Result<BTreeMap<String, Summary>> {
    let mut q = Sql::new("SELECT c.s, ");
    q.add(entity::EVIDENCE_COLS).add(" FROM claims c WHERE c.ent = 1");
    if let Some(only) = only {
        q.add(" AND c.s IN (SELECT value FROM json_each(").arg(json_list(only)).add("))");
    }
    v.sql(&mut q, "c");
    q.add(" ORDER BY c.s");
    let mut out = BTreeMap::new();
    let mut stmt = conn.prepare(&q.text).map_err(db)?;
    let mut rows = stmt.query(params_from_iter(q.params.iter())).map_err(db)?;
    // rows arrive grouped by subject; each group is summarized as it closes
    let mut group: Option<(String, Vec<entity::Evidence>)> = None;
    while let Some(row) = rows.next().map_err(db)? {
        let s: String = row.get(0).map_err(db)?;
        let ev = entity::evidence_at(row, 1)?;
        match &mut group {
            Some((gs, evs)) if *gs == s => evs.push(ev),
            _ => {
                if let Some((gs, evs)) = group.replace((s, vec![ev])) {
                    out.insert(gs, entity::summarize(evs));
                }
            }
        }
    }
    if let Some((gs, evs)) = group {
        out.insert(gs, entity::summarize(evs));
    }
    Ok(out)
}

fn subjects(conn: &Connection, q: &Sql) -> Result<BTreeSet<String>> {
    Ok(rows(conn, q, |r| r.get::<_, String>(0).map_err(db))?.into_iter().collect())
}

/// A matching entity with its summary under the view.
struct Hit {
    id: String,
    summary: Summary,
    score: Option<f64>,
    distance: Option<f64>,
}

impl Hit {
    fn keys(&self) -> Keys<'_> {
        Keys {
            id: &self.id,
            score: self.score,
            when: self.summary.when.as_ref().and_then(|w| w.key),
            recorded: self.summary.first_at,
            distance: self.distance,
        }
    }

    fn view(&self, space: &Id) -> Result<EntityView> {
        let mut e = self.summary.view(&self.id, space)?;
        e.score = self.score;
        e.distance_km = self.distance;
        Ok(e)
    }
}

fn point(s: &Summary) -> Option<(f64, f64)> {
    s.point.map(|(lat, lon, _)| (lat, lon))
}

/// Computed route: entities matching `w`'s entity-level filters (type, text, when,
/// near) through claims passing `v`, in id order. For `find_entities`
/// (`for_entities`), also `w.ids` and the claim-level filters: some passing claim
/// about the entity satisfies all of them.
fn computed_hits(conn: &Connection, w: &WhereF, v: &ViewF, scores: Option<&BTreeMap<String, f64>>, for_entities: bool) -> Result<Vec<Hit>> {
    let mut only: Option<BTreeSet<String>> = None;
    let mut restrict = |set: BTreeSet<String>| {
        only = Some(match only.take() {
            Some(o) => o.intersection(&set).cloned().collect(),
            None => set,
        });
    };
    if for_entities && !w.ids.is_empty() {
        restrict(w.ids.iter().cloned().collect());
    }
    if let Some(s) = scores {
        restrict(s.keys().cloned().collect());
    }
    if !w.types.is_empty() {
        let mut q = Sql::new("");
        w.type_subjects_sql(&mut q, v);
        restrict(subjects(conn, &q)?);
    }
    if for_entities && w.has_claim_filters() {
        let mut q = Sql::new("SELECT c.s FROM claims c WHERE c.ent = 1");
        v.sql(&mut q, "c");
        w.claim_sql(&mut q, "c");
        restrict(subjects(conn, &q)?);
    }
    if only.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for (id, summary) in computed_summaries(conn, v, only.as_ref())? {
        if let Some((lo, hi)) = w.when
            && !summary.when_overlaps(lo, hi)
        {
            continue;
        }
        let distance = match &w.near {
            None => None,
            Some(n) => match n.within(point(&summary)) {
                Some(d) => Some(d),
                None => continue,
            },
        };
        let score = scores.and_then(|s| s.get(&id).copied());
        hits.push(Hit { id, summary, score, distance });
    }
    Ok(hits)
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
    let scores = w.terms.as_deref().map(|t| text_scores(conn, t, &v)).transpose()?;

    let hits = if v.is_default() {
        if scores.as_ref().is_some_and(BTreeMap::is_empty) {
            return Ok(Page { items: Vec::new(), cursor: None });
        }
        let mut q = Sql::new("SELECT ");
        q.add(entity::COLS).add(" FROM entities e WHERE 1");
        if !w.ids.is_empty() {
            q.add(" AND e.id IN ").list(w.ids.iter().cloned());
        }
        if let Some(s) = &scores {
            q.add(" AND e.id IN (SELECT value FROM json_each(").arg(json_list(s.keys())).add("))");
        }
        w.stored_sql(&mut q, &v);
        q.add(" AND EXISTS (SELECT 1 FROM claims c WHERE c.s = e.id AND c.ent = 1");
        v.sql(&mut q, "c");
        w.claim_sql(&mut q, "c");
        q.add(")");
        if scores.is_none()
            && w.near.is_none()
            && let Some(order_by) = sql_order(sort, "e.when_key", "e.first_at", "e.id")
        {
            q.add(&order_by).add(" LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
            let items = rows(conn, &q, |row| {
                let (id, summary) = entity::stored_at(row, 0)?;
                summary.view(&id, space)
            })?;
            return page(items, offset, limit);
        }
        let mut hits = Vec::new();
        for (id, summary) in rows(conn, &q, |row| entity::stored_at(row, 0))? {
            let distance = match &w.near {
                None => None,
                Some(n) => match n.within(point(&summary)) {
                    Some(d) => Some(d),
                    None => continue,
                },
            };
            let score = scores.as_ref().and_then(|s| s.get(&id).copied());
            hits.push(Hit { id, summary, score, distance });
        }
        hits
    } else {
        computed_hits(conn, &w, &v, scores.as_ref(), true)?
    };
    let page = sorted_page(hits, |a, b| a.keys().order(&b.keys(), sort), offset, limit)?;
    Ok(Page { items: page.items.iter().map(|h| h.view(space)).collect::<Result<_>>()?, cursor: page.cursor })
}

/// A matching claim with the order keys of its subject entity.
struct ClaimHit {
    view: ClaimView,
    id: String,
    at: i64,
    when: Option<i64>,
    score: Option<f64>,
    distance: Option<f64>,
}

impl ClaimHit {
    fn keys(&self) -> Keys<'_> {
        Keys { id: &self.id, score: self.score, when: self.when, recorded: Some(self.at), distance: self.distance }
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
    // The claim-level part, shared by every route: the claim itself passes the view
    // and the claim filters.
    let claim_conditions = |q: &mut Sql| {
        if !w.ids.is_empty() {
            q.add(" AND c.id IN ").list(w.ids.iter().cloned());
        }
        v.sql(q, "c");
        w.claim_sql(q, "c");
    };

    // Entity-level filters (type, text, when, near) and the `when` order apply to the
    // claim's subject entity, under the same view. Without either, it is not needed.
    if !w.has_entity_filters()
        && let Sort::Recorded { desc } = sort
    {
        let mut q = Sql::new("SELECT c.json, c.status, c.stale FROM claims c WHERE 1");
        claim_conditions(&mut q);
        q.add(&recorded_order(desc, "c.at", "c.id")).add(" LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
        return page(rows(conn, &q, |row| claim_view_at(row, 0))?, offset, limit);
    }
    let scores = w.terms.as_deref().map(|t| text_scores(conn, t, &v)).transpose()?;
    if scores.as_ref().is_some_and(BTreeMap::is_empty) {
        return Ok(Page { items: Vec::new(), cursor: None });
    }

    let mut q = Sql::new("SELECT c.json, c.status, c.stale, c.id, c.at, c.s");
    let hits = if v.is_default() {
        q.add(", e.when_key, e.lat, e.lon FROM claims c LEFT JOIN entities e ON e.id = c.s WHERE 1");
        claim_conditions(&mut q);
        if let Some(s) = &scores {
            q.add(" AND c.s IN (SELECT value FROM json_each(").arg(json_list(s.keys())).add("))");
        }
        w.stored_sql(&mut q, &v);
        if scores.is_none()
            && w.near.is_none()
            && let Some(order_by) = sql_order(sort, "e.when_key", "c.at", "c.id")
        {
            q.add(&order_by).add(" LIMIT ").arg(limit + 1).add(" OFFSET ").arg(i64_of(offset, "cursor offset")?);
            return page(rows(conn, &q, |row| claim_view_at(row, 0))?, offset, limit);
        }
        let mut hits = Vec::new();
        let fetched = rows(conn, &q, |row| {
            let lat: Option<f64> = row.get(7).map_err(db)?;
            let lon: Option<f64> = row.get(8).map_err(db)?;
            Ok((claim_view_at(row, 0)?, row.get::<_, String>(3).map_err(db)?, row.get::<_, i64>(4).map_err(db)?, row.get::<_, String>(5).map_err(db)?, row.get::<_, Option<i64>>(6).map_err(db)?, lat.zip(lon)))
        })?;
        for (view, id, at, s, when, point) in fetched {
            let distance = match &w.near {
                None => None,
                Some(n) => match n.within(point) {
                    Some(d) => Some(d),
                    None => continue,
                },
            };
            let score = scores.as_ref().and_then(|m| m.get(&s).copied());
            hits.push(ClaimHit { view, id, at, when, score, distance });
        }
        hits
    } else {
        let matched: Option<BTreeMap<String, Hit>> = if w.has_entity_filters() {
            Some(computed_hits(conn, &w, &v, scores.as_ref(), false)?.into_iter().map(|h| (h.id.clone(), h)).collect())
        } else {
            None
        };
        if matched.as_ref().is_some_and(BTreeMap::is_empty) {
            return Ok(Page { items: Vec::new(), cursor: None });
        }
        q.add(" FROM claims c WHERE 1");
        claim_conditions(&mut q);
        if let Some(m) = &matched {
            q.add(" AND c.s IN (SELECT value FROM json_each(").arg(json_list(m.keys())).add("))");
        }
        let fetched = rows(conn, &q, |row| {
            Ok((claim_view_at(row, 0)?, row.get::<_, String>(3).map_err(db)?, row.get::<_, i64>(4).map_err(db)?, row.get::<_, String>(5).map_err(db)?))
        })?;
        let mut hits = Vec::with_capacity(fetched.len());
        match &matched {
            Some(m) => {
                for (view, id, at, s) in fetched {
                    let hit = m.get(&s).ok_or_else(|| Error::internal(format!("claim {id} matched outside its subject set")))?;
                    hits.push(ClaimHit { view, id, at, when: hit.keys().when, score: hit.score, distance: hit.distance });
                }
            }
            // no entity-level filter: only the `when` order reads the subjects
            None => {
                let subjects: BTreeSet<String> = fetched.iter().map(|f| f.3.clone()).collect();
                let summaries = computed_summaries(conn, &v, Some(&subjects))?;
                for (view, id, at, s) in fetched {
                    let when = summaries.get(&s).and_then(|x| x.when.as_ref()).and_then(|x| x.key);
                    hits.push(ClaimHit { view, id, at, when, score: None, distance: None });
                }
            }
        }
        hits
    };
    let page = sorted_page(hits, |a, b| a.keys().order(&b.keys(), sort), offset, limit)?;
    Ok(Page { items: page.items.into_iter().map(|h| h.view).collect(), cursor: page.cursor })
}

pub(crate) fn entity(conn: &Connection, space: &Id, id: &Id, view: &View) -> Result<Option<EntityView>> {
    let v = ViewF::parse(view)?;
    if v.is_default() {
        let mut q = Sql::new("SELECT ");
        q.add(entity::COLS).add(" FROM entities e WHERE e.id = ").arg(id.as_str().to_string());
        q.add(" AND EXISTS (SELECT 1 FROM claims c WHERE c.s = e.id AND c.ent = 1");
        v.sql(&mut q, "c");
        q.add(")");
        return rows(conn, &q, |row| entity::stored_at(row, 0))?.into_iter().next().map(|(id, s)| s.view(&id, space)).transpose();
    }
    let only = BTreeSet::from([id.as_str().to_string()]);
    computed_summaries(conn, &v, Some(&only))?.remove(id.as_str()).map(|s| s.view(id.as_str(), space)).transpose()
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

/// DESIGN §4.4: only an active (accepted|proposed), non-stale equivalent blocks a
/// write; the earliest by seq if there are several.
pub(crate) fn equivalent(conn: &Connection, actor: &Id, equiv_key: &str) -> Result<Option<ClaimView>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT json, status, stale FROM claims
             WHERE equiv = ?1 AND actor = ?2 AND status IN (?3, ?4) AND stale = 0 ORDER BY seq LIMIT 1",
        )
        .map_err(db)?;
    let mut rows = stmt.query([equiv_key, actor.as_str(), Status::Accepted.as_str(), Status::Proposed.as_str()]).map_err(db)?;
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
