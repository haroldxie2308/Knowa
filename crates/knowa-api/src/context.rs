//! `context` (DESIGN §6.2): an entity with its claims, inbound references, graph
//! neighborhood, temporal and spatial neighbors and the actors involved, composed
//! from single-replica index primitives across the selected spaces.

use std::collections::HashMap;

use knowa_config::Home;
use knowa_model::query::{ContextDoc, ContextResult, MAX_HOPS, MAX_LIMIT, Near, Neighbor, Order, View, Where, WhenRange};
use knowa_model::time::parse_duration_ms;
use knowa_model::{ActorInfo, ClaimView, Dec, Edge, EntityView, Error, Id, Result, Timestamp};

use crate::Sel;
use crate::merge::{ClaimSort, merge_entities, midpoint, sort_claims, stub_entity, when_bounds, when_center};
use crate::query::{limit_or, merged_entity, reject_as_of};
use crate::select::{Opened, open_all, select};

/// ContextDoc docs: per-section cap default.
const DEFAULT_SECTION_LIMIT: u32 = 20;
/// ContextDoc docs: graph depth default.
const DEFAULT_HOPS: u32 = 1;
/// ContextDoc docs: temporal window default.
const DEFAULT_WINDOW: &str = "P1D";
/// ContextDoc docs: spatial radius default (km).
const DEFAULT_KM: &str = "1";
/// Temporal candidates are ranked by distance to the anchor's center, which no index
/// order provides, so every overlapping entity is read. Past this many the call
/// fails (narrow `window`) instead of ranking a silently truncated set.
pub(crate) const MAX_TEMPORAL_SCAN: usize = 50_000;

struct Params {
    hops: u32,
    window_ms: i64,
    km: String,
    limit: u32,
}

fn validate(d: &ContextDoc) -> Result<Params> {
    reject_as_of(&d.as_of)?;
    let hops = d.hops.unwrap_or(DEFAULT_HOPS);
    if hops > MAX_HOPS {
        return Err(Error::invalid(format!("hops must be at most {MAX_HOPS}, got {hops}")));
    }
    let window = d.window.as_deref().unwrap_or(DEFAULT_WINDOW);
    let window_ms = parse_duration_ms(window).map_err(|e| Error::invalid(format!("window: {}", e.message)))?;
    let km = d.km.clone().unwrap_or_else(|| DEFAULT_KM.to_string());
    let km_dec = Dec::parse(&km).map_err(|e| Error::invalid(format!("km: {}", e.message)))?;
    if km_dec.to_f64() < 0.0 {
        return Err(Error::invalid(format!("km must not be negative, got {km}")));
    }
    let limit = limit_or(d.limit, DEFAULT_SECTION_LIMIT, "limit")?;
    Ok(Params { hops, window_ms, km, limit })
}

pub(crate) fn context(home: &Home, sel: &Sel, d: ContextDoc) -> Result<ContextResult> {
    let p = validate(&d)?;
    let opened = open_all(select(home, sel, &d.spaces)?)?;
    let view = &d.view;
    let anchor = merged_entity(&opened, &d.id, view)?;

    let mut claims = Vec::new();
    let mut inbound = Vec::new();
    for o in &opened {
        let (out, inc) = o.index.claims_about(&d.id, view, p.limit)?;
        claims.extend(out);
        inbound.extend(inc);
    }
    for list in [&mut claims, &mut inbound] {
        sort_claims(list, ClaimSort::RecordedAsc);
        list.truncate(p.limit as usize);
    }
    if anchor.is_none() && claims.is_empty() && inbound.is_empty() {
        return Err(Error::not_found(format!("context: nothing in the selected spaces describes or references {}", d.id)));
    }
    // An id known only through references (e.g. an `x:` key) has no entity row.
    let entity = anchor.unwrap_or_else(|| stub_entity(&d.id));

    let hits = neighbors(&d.id, p.hops, p.limit as usize, |id| edges_all(&opened, id, view))?;
    let mut views: HashMap<Id, EntityView> = HashMap::new();
    let mut neighbor_list = Vec::with_capacity(hits.len());
    for h in hits {
        let ev = match views.get(&h.edge.to) {
            Some(v) => v.clone(),
            None => {
                let v = merged_entity(&opened, &h.edge.to, view)?.unwrap_or_else(|| stub_entity(&h.edge.to));
                views.insert(h.edge.to.clone(), v.clone());
                v
            }
        };
        neighbor_list.push(Neighbor { entity: ev, via: h.edge.via, p: h.edge.p, dir: h.edge.dir, hop: h.hop });
    }

    let temporal = temporal(&opened, &entity, view, p.window_ms, p.limit)?;
    let spatial = spatial(&opened, &entity, view, &p.km, p.limit)?;
    let actors = actors(&opened, claims.iter().chain(&inbound))?;
    Ok(ContextResult { entity, claims, inbound, neighbors: neighbor_list, temporal, spatial, actors })
}

/// Edges touching `id` in every selected space (claims are space-specific, so the
/// lists never overlap).
pub(crate) fn edges_all(opened: &[Opened], id: &Id, view: &View) -> Result<Vec<Edge>> {
    let mut out = Vec::new();
    for o in opened {
        out.extend(o.index.edges(id, view)?);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hit {
    pub edge: Edge,
    pub hop: u32,
}

/// Breadth-first neighborhood up to `hops`, at most `cap` entries. Every edge from a
/// node at depth h−1 to a node first reached at depth h is listed (so a node reached
/// by two edges appears twice, once per linking claim); edges back to the anchor or
/// to shallower nodes are not. Each newly reached node yields an entry, so a frontier
/// never exceeds `cap` and the number of `edges` calls stays ≤ 1 + cap·hops.
pub(crate) fn neighbors(
    anchor: &Id,
    hops: u32,
    cap: usize,
    mut edges: impl FnMut(&Id) -> Result<Vec<Edge>>,
) -> Result<Vec<Hit>> {
    let mut depth: HashMap<Id, u32> = HashMap::from([(anchor.clone(), 0)]);
    let mut frontier = vec![anchor.clone()];
    let mut out = Vec::new();
    for hop in 1..=hops {
        let mut next = Vec::new();
        for u in &frontier {
            for e in edges(u)? {
                match depth.get(&e.to) {
                    Some(&d) if d < hop => continue,
                    Some(_) => {}
                    None => {
                        depth.insert(e.to.clone(), hop);
                        next.push(e.to.clone());
                    }
                }
                if out.len() == cap {
                    return Ok(out);
                }
                out.push(Hit { edge: e, hop });
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(out)
}

/// Entities whose `when` overlaps the anchor's `[lo, hi)` widened by `window` on both
/// sides, excluding the anchor, nearest center first. Needs a finite anchor range.
fn temporal(opened: &[Opened], anchor: &EntityView, view: &View, window_ms: i64, limit: u32) -> Result<Vec<EntityView>> {
    let Some(w) = &anchor.when else { return Ok(Vec::new()) };
    let (Some(lo), Some(hi)) = when_bounds(w)? else { return Ok(Vec::new()) };
    let center = midpoint(lo, hi);
    // WhenRange is EDTF: a ms instant `t` parses to [t, t+1), so `to` is the last
    // included millisecond.
    let widened = lo.checked_sub(window_ms).zip(hi.checked_add(window_ms - 1));
    let Some((from, to)) = widened else {
        return Err(Error::invalid(format!("window of {window_ms} ms is too large")));
    };
    let range = WhenRange { from: Some(Timestamp(from).format()), to: Some(Timestamp(to).format()) };
    let filter = Where { when: Some(range), ..Where::default() };
    let mut lists = Vec::with_capacity(opened.len());
    let mut scanned = 0usize;
    for o in opened {
        let mut list = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = o.index.find_entities(&filter, view, Order::When, MAX_LIMIT, cursor.as_deref())?;
            scanned += page.items.len();
            if scanned > MAX_TEMPORAL_SCAN {
                return Err(Error::unsupported(format!(
                    "context: more than {MAX_TEMPORAL_SCAN} entities overlap the temporal window; use a smaller window"
                ))
                .with_detail(serde_json::json!({ "scanned": scanned })));
            }
            list.extend(page.items);
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        lists.push(list);
    }
    let mut keyed = Vec::new();
    for e in merge_entities(lists)? {
        if e.id == anchor.id {
            continue;
        }
        let gap = when_center(e.when.as_ref())?.map(|c| c.abs_diff(center));
        keyed.push((gap, e));
    }
    // Entities without a center (open-ended ranges) go last.
    keyed.sort_by(|(a, x), (b, y)| match (a, b) {
        (Some(a), Some(b)) => a.cmp(b).then_with(|| x.id.cmp(&y.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => x.id.cmp(&y.id),
    });
    Ok(keyed.into_iter().take(limit as usize).map(|(_, e)| e).collect())
}

/// Entities within `km` of the anchor's `where`, excluding the anchor, nearest first.
fn spatial(opened: &[Opened], anchor: &EntityView, view: &View, km: &str, limit: u32) -> Result<Vec<EntityView>> {
    let Some(w) = &anchor.where_ else { return Ok(Vec::new()) };
    let filter = Where {
        near: Some(Near { lat: w.lat.to_string(), lon: w.lon.to_string(), km: km.to_string() }),
        ..Where::default()
    };
    let mut lists = Vec::with_capacity(opened.len());
    for o in opened {
        let page = o.index.find_entities(&filter, view, Order::Distance, limit, None)?;
        let mut items = page.items;
        // The anchor is usually its own nearest hit; when it took a slot, read one more
        // so the section still holds `limit` others.
        if items.iter().any(|e| e.id == anchor.id)
            && let Some(c) = page.cursor
        {
            items.extend(o.index.find_entities(&filter, view, Order::Distance, 1, Some(&c))?.items);
        }
        lists.push(items);
    }
    let mut items: Vec<EntityView> = merge_entities(lists)?.into_iter().filter(|e| e.id != anchor.id).collect();
    items.sort_by(|a, b| match (a.distance_km, b.distance_km) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    items.truncate(limit as usize);
    Ok(items)
}

/// Authors of the context's claims. Kind, role and autoAccept come from the first
/// selected space that knows the actor (roles are per space); `claims` counts the
/// actor's claims shown in this context. Most active first.
fn actors<'a>(opened: &[Opened], shown: impl Iterator<Item = &'a ClaimView>) -> Result<Vec<ActorInfo>> {
    let mut counts: Vec<(Id, u64)> = Vec::new();
    for c in shown {
        let a = &c.claim.body.actor;
        match counts.iter_mut().find(|(id, _)| id == a) {
            Some((_, n)) => *n += 1,
            None => counts.push((a.clone(), 1)),
        }
    }
    if counts.is_empty() {
        return Ok(Vec::new());
    }
    let mut known: HashMap<Id, ActorInfo> = HashMap::new();
    for o in opened {
        for info in o.index.actors()? {
            known.entry(info.id.clone()).or_insert(info);
        }
    }
    let mut out = Vec::with_capacity(counts.len());
    for (id, n) in counts {
        let info = known.get(&id).ok_or_else(|| {
            Error::internal(format!("context: index lists no actor info for {id}, which authored a returned claim"))
        })?;
        out.push(ActorInfo { claims: n, ..info.clone() });
    }
    out.sort_by(|a, b| b.claims.cmp(&a.claims).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_graph::{edges_of, node};
    use knowa_model::Dir;

    #[test]
    fn neighbors_by_hop_without_back_edges() {
        // 0 - 1 - 2 - 3, and 0 - 2 via a second claim
        let g = [(0, 1, 1), (1, 2, 2), (2, 3, 3), (2, 0, 4)];
        let hits = neighbors(&node(0), 2, 20, |n| Ok(edges_of(&g, n))).unwrap();
        let got: Vec<(Id, u32, Dir)> = hits.iter().map(|h| (h.edge.to.clone(), h.hop, h.edge.dir)).collect();
        assert_eq!(got, vec![(node(1), 1, Dir::Out), (node(2), 1, Dir::In), (node(3), 2, Dir::Out)]);
    }

    #[test]
    fn neighbors_lists_each_linking_edge_into_a_new_node() {
        // two different claims link 0 and 1
        let g = [(0, 1, 1), (1, 0, 2)];
        let hits = neighbors(&node(0), 1, 20, |n| Ok(edges_of(&g, n))).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.edge.to == node(1) && h.hop == 1));
    }

    #[test]
    fn neighbors_zero_hops_is_empty() {
        let g = [(0, 1, 1)];
        assert!(neighbors(&node(0), 0, 20, |n| Ok(edges_of(&g, n))).unwrap().is_empty());
    }

    #[test]
    fn neighbors_respects_cap() {
        let g: Vec<_> = (1..=10).map(|i| (0, i, i)).collect();
        let hits = neighbors(&node(0), 3, 4, |n| Ok(edges_of(&g, n))).unwrap();
        assert_eq!(hits.len(), 4);
    }

    #[test]
    fn neighbors_propagates_edge_errors() {
        let e = neighbors(&node(0), 1, 4, |_| Err(Error::internal("boom"))).unwrap_err();
        assert_eq!(e.message, "boom");
    }
}
