//! Multi-space merging of per-index results (DESIGN §8: fan out, merge in the api).
//!
//! A single selected space never goes through here: its index's own order and
//! cursor are returned unchanged.

use std::cmp::Ordering;
use std::collections::HashMap;

use knowa_model::query::Order;
use knowa_model::view::{EntityWhen, WhenSource};
use knowa_model::{ClaimView, EntityView, Error, Id, Result, Timestamp};

/// Merges entity lists from several spaces (in selection order) by id, keeping
/// first-appearance order: `spaces`, `types` and supporting `claims` are unioned;
/// `label` and `where` come from the first space that has them (`distanceKm` travels
/// with the `where` it was measured from); `when` as `merge_when`; `score` is the best.
pub(crate) fn merge_entities(lists: Vec<Vec<EntityView>>) -> Result<Vec<EntityView>> {
    let mut out: Vec<EntityView> = Vec::new();
    let mut pos: HashMap<Id, usize> = HashMap::new();
    for list in lists {
        for e in list {
            match pos.get(&e.id) {
                Some(&i) => absorb(&mut out[i], e)?,
                None => {
                    pos.insert(e.id.clone(), out.len());
                    out.push(e);
                }
            }
        }
    }
    Ok(out)
}

fn absorb(into: &mut EntityView, e: EntityView) -> Result<()> {
    union(&mut into.spaces, e.spaces);
    union(&mut into.types, e.types);
    union(&mut into.claims, e.claims);
    if into.label.is_none() {
        into.label = e.label;
    }
    into.when = merge_when(into.when.take(), e.when)?;
    if into.where_.is_none() {
        into.where_ = e.where_;
        into.distance_km = e.distance_km;
    }
    into.score = match (into.score, e.score) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    Ok(())
}

/// `first` is from an earlier space in selection order than `later`. A `knowa:when`
/// (`source: claim`) is evidence and `source: recorded` only the DESIGN §4.8 fallback,
/// so evidence wins whatever the order. Between two `knowa:when`s the first space wins,
/// as for the other display fields. Between two fallbacks the earlier time wins: the
/// fallback is the earliest record time of the entity's claims, and the merged entity
/// has the claims of both spaces.
fn merge_when(first: Option<EntityWhen>, later: Option<EntityWhen>) -> Result<Option<EntityWhen>> {
    Ok(match (first, later) {
        (Some(a), Some(b)) => Some(match (a.source, b.source) {
            (WhenSource::Recorded, WhenSource::Claim) => b,
            (WhenSource::Recorded, WhenSource::Recorded) if when_bounds(&b)?.0 < when_bounds(&a)?.0 => b,
            _ => a,
        }),
        (a, b) => a.or(b),
    })
}

fn union<T: PartialEq>(into: &mut Vec<T>, from: Vec<T>) {
    for x in from {
        if !into.contains(&x) {
            into.push(x);
        }
    }
}

/// An entity view with only its id: a graph node no selected space describes.
pub(crate) fn stub_entity(id: &Id) -> EntityView {
    EntityView {
        id: id.clone(),
        spaces: Vec::new(),
        label: None,
        types: Vec::new(),
        when: None,
        where_: None,
        score: None,
        distance_km: None,
        claims: Vec::new(),
    }
}

/// How merged entities are ordered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EntitySort {
    ScoreDesc,
    WhenAsc,
    WhenDesc,
    DistanceAsc,
}

/// Orders that can be re-established after merging entity lists. `EntityView`
/// carries no record time, so `recorded`/`-recorded` cannot be merged.
pub(crate) fn entity_sort(order: Order, has_text: bool) -> Result<EntitySort> {
    Ok(match order {
        Order::Relevance if has_text => EntitySort::ScoreDesc,
        // Order docs: relevance without text is `-when` for entities.
        Order::Relevance | Order::WhenDesc => EntitySort::WhenDesc,
        Order::When => EntitySort::WhenAsc,
        Order::Distance => EntitySort::DistanceAsc,
        Order::Recorded | Order::RecordedDesc => {
            return Err(Error::unsupported(
                "multi-space find of entities ordered by record time is not supported in v0; select one space, or use target: claims",
            ));
        }
    })
}

/// How merged claims are ordered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ClaimSort {
    RecordedAsc,
    RecordedDesc,
}

/// Orders that can be re-established after merging claim lists. A `ClaimView` has no
/// relevance score, distance or when key, so only record-time orders merge.
pub(crate) fn claim_sort(order: Order, has_text: bool) -> Result<ClaimSort> {
    match order {
        Order::Recorded => Ok(ClaimSort::RecordedAsc),
        // Order docs: relevance without text is `-recorded` for claims.
        Order::RecordedDesc => Ok(ClaimSort::RecordedDesc),
        Order::Relevance if !has_text => Ok(ClaimSort::RecordedDesc),
        _ => Err(Error::unsupported(
            "multi-space find of claims supports only record-time order (recorded, -recorded, or relevance without text) in v0; select one space",
        )),
    }
}

/// Stable sort; entities lacking the sort key go last in either direction.
pub(crate) fn sort_entities(items: Vec<EntityView>, sort: EntitySort) -> Result<Vec<EntityView>> {
    let mut keyed = Vec::with_capacity(items.len());
    for e in items {
        let k = match sort {
            EntitySort::ScoreDesc => e.score.map(|s| -s),
            EntitySort::DistanceAsc => e.distance_km,
            EntitySort::WhenAsc => when_center(e.when.as_ref())?.map(|c| c as f64),
            EntitySort::WhenDesc => when_center(e.when.as_ref())?.map(|c| -(c as f64)),
        };
        keyed.push((k, e));
    }
    keyed.sort_by(|(a, _), (b, _)| missing_last(*a, *b));
    Ok(keyed.into_iter().map(|(_, e)| e).collect())
}

fn missing_last(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Midpoint of an entity's `[from, to)` (cf. `When::center`); with one open end the
/// closed end stands in, so half-open ranges still sort near their known edge.
pub(crate) fn when_center(w: Option<&EntityWhen>) -> Result<Option<i64>> {
    let Some(w) = w else { return Ok(None) };
    let (lo, hi) = when_bounds(w)?;
    Ok(match (lo, hi) {
        (Some(l), Some(h)) => Some(midpoint(l, h)),
        (Some(l), None) => Some(l),
        (None, Some(h)) => Some(h),
        (None, None) => None,
    })
}

/// Floored midpoint of `[lo, hi)`, as `When::center` computes it.
pub(crate) fn midpoint(lo: i64, hi: i64) -> i64 {
    lo + (hi - lo) / 2
}

/// `[from, to)` in Unix ms; `None` = open.
pub(crate) fn when_bounds(w: &EntityWhen) -> Result<(Option<i64>, Option<i64>)> {
    let parse = |s: &Option<String>| -> Result<Option<i64>> {
        s.as_deref()
            .map(|t| {
                Timestamp::parse(t)
                    .map(|ts| ts.0)
                    .map_err(|e| Error::internal(format!("index returned a malformed entity when bound {t:?}: {}", e.message)))
            })
            .transpose()
    };
    Ok((parse(&w.from)?, parse(&w.to)?))
}

pub(crate) fn sort_claims(items: &mut [ClaimView], sort: ClaimSort) {
    let key = |c: &ClaimView| (c.claim.body.at, c.claim.id.clone());
    match sort {
        ClaimSort::RecordedAsc => items.sort_by_key(key),
        ClaimSort::RecordedDesc => items.sort_by_key(|c| std::cmp::Reverse(key(c))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::view::EntityWhere;
    use knowa_model::{ErrorCode, Term};

    fn id(s: &str) -> Id {
        Id::parse(s).unwrap()
    }

    const E1: &str = "e_01arz3ndektsv4rrffq69g5fa1";
    const E2: &str = "e_01arz3ndektsv4rrffq69g5fa2";
    const E3: &str = "e_01arz3ndektsv4rrffq69g5fa3";
    const S1: &str = "s_01arz3ndektsv4rrffq69g5fa1";
    const S2: &str = "s_01arz3ndektsv4rrffq69g5fa2";

    fn ent(e: &str, space: &str) -> EntityView {
        let mut v = stub_entity(&id(e));
        v.spaces = vec![id(space)];
        v
    }

    fn when(from: Option<i64>, to: Option<i64>) -> EntityWhen {
        EntityWhen {
            value: "x".into(),
            from: from.map(|t| Timestamp(t).format()),
            to: to.map(|t| Timestamp(t).format()),
            source: WhenSource::Claim,
        }
    }

    #[test]
    fn merge_unions_and_takes_first_present_display_fields() {
        let mut a = ent(E1, S1);
        a.types = vec![Term::parse("knowa:Photo").unwrap()];
        a.score = Some(1.0);
        let mut b = ent(E1, S2);
        b.label = Some("from b".into());
        b.types = vec![Term::parse("knowa:Photo").unwrap(), Term::parse("knowa:File").unwrap()];
        b.where_ = Some(EntityWhere { lat: 1.0, lon: 2.0, r: None });
        b.distance_km = Some(0.5);
        b.score = Some(3.0);
        let mut c = ent(E2, S2);
        c.label = Some("other".into());

        let merged = merge_entities(vec![vec![a], vec![c, b]]).unwrap();
        assert_eq!(merged.len(), 2);
        let m = &merged[0];
        assert_eq!(m.id, id(E1));
        assert_eq!(m.spaces, vec![id(S1), id(S2)]);
        assert_eq!(m.types.len(), 2);
        assert_eq!(m.label.as_deref(), Some("from b"));
        assert_eq!(m.distance_km, Some(0.5));
        assert_eq!(m.score, Some(3.0));
        assert_eq!(merged[1].id, id(E2));
    }

    #[test]
    fn merge_keeps_first_space_label_when_both_have_one() {
        let mut a = ent(E1, S1);
        a.label = Some("first".into());
        let mut b = ent(E1, S2);
        b.label = Some("second".into());
        let merged = merge_entities(vec![vec![a], vec![b]]).unwrap();
        assert_eq!(merged[0].label.as_deref(), Some("first"));
    }

    fn recorded(at: i64) -> EntityWhen {
        EntityWhen { source: WhenSource::Recorded, ..when(Some(at), Some(at + 1)) }
    }

    fn merged_when(first: Option<EntityWhen>, later: Option<EntityWhen>) -> Option<EntityWhen> {
        let mut a = ent(E1, S1);
        a.when = first;
        let mut b = ent(E1, S2);
        b.when = later;
        merge_entities(vec![vec![a], vec![b]]).unwrap().remove(0).when
    }

    #[test]
    fn a_when_claim_beats_a_record_time_fallback_in_any_space_order() {
        let claimed = when(Some(0), Some(100));
        assert_eq!(merged_when(Some(recorded(5_000)), Some(claimed.clone())), Some(claimed.clone()));
        assert_eq!(merged_when(Some(claimed.clone()), Some(recorded(5_000))), Some(claimed.clone()));
        // Two claims: the first space wins, as for the other display fields.
        let other = when(Some(200), Some(300));
        assert_eq!(merged_when(Some(other.clone()), Some(claimed.clone())), Some(other));
        // Two fallbacks: the earliest record time, which is what one space holding
        // both spaces' claims would report.
        assert_eq!(merged_when(Some(recorded(9_000)), Some(recorded(5_000))), Some(recorded(5_000)));
        assert_eq!(merged_when(Some(recorded(5_000)), Some(recorded(9_000))), Some(recorded(5_000)));
        assert_eq!(merged_when(None, Some(recorded(5_000))), Some(recorded(5_000)));
    }

    #[test]
    fn sort_by_score_desc_missing_last() {
        let mut a = ent(E1, S1);
        a.score = Some(1.0);
        let b = ent(E2, S1);
        let mut c = ent(E3, S1);
        c.score = Some(5.0);
        let out = sort_entities(vec![a, b, c], EntitySort::ScoreDesc).unwrap();
        let ids: Vec<_> = out.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, vec![id(E3), id(E1), id(E2)]);
    }

    #[test]
    fn sort_by_when_uses_center_and_open_ends() {
        let mut a = ent(E1, S1);
        a.when = Some(when(Some(0), Some(100))); // center 50
        let mut b = ent(E2, S1);
        b.when = Some(when(Some(20), None)); // open: 20
        let c = ent(E3, S1); // no when
        let asc = sort_entities(vec![c.clone(), a.clone(), b.clone()], EntitySort::WhenAsc).unwrap();
        assert_eq!(asc.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec![id(E2), id(E1), id(E3)]);
        let desc = sort_entities(vec![c, a, b], EntitySort::WhenDesc).unwrap();
        assert_eq!(desc.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec![id(E1), id(E2), id(E3)]);
    }

    #[test]
    fn sort_is_stable_for_ties() {
        let mut a = ent(E1, S1);
        a.distance_km = Some(1.0);
        let mut b = ent(E2, S2);
        b.distance_km = Some(1.0);
        let out = sort_entities(vec![a, b], EntitySort::DistanceAsc).unwrap();
        assert_eq!(out[0].id, id(E1));
    }

    #[test]
    fn unmergeable_orders_are_unsupported() {
        assert_eq!(entity_sort(Order::Recorded, false).unwrap_err().code, ErrorCode::Unsupported);
        assert_eq!(claim_sort(Order::Relevance, true).unwrap_err().code, ErrorCode::Unsupported);
        assert_eq!(claim_sort(Order::Distance, false).unwrap_err().code, ErrorCode::Unsupported);
        assert_eq!(claim_sort(Order::When, false).unwrap_err().code, ErrorCode::Unsupported);
    }

    #[test]
    fn relevance_defaults_follow_order_docs() {
        assert_eq!(entity_sort(Order::Relevance, false).unwrap(), EntitySort::WhenDesc);
        assert_eq!(entity_sort(Order::Relevance, true).unwrap(), EntitySort::ScoreDesc);
        assert_eq!(claim_sort(Order::Relevance, false).unwrap(), ClaimSort::RecordedDesc);
    }
}
