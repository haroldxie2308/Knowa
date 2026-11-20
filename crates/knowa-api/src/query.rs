//! The query primitive (DESIGN §6.1, §6.2): validation, fan-out to each selected
//! replica's index, and merging. `context` and `path` live in their own modules.

use std::fs;

use knowa_config::Home;
use knowa_model::query::{
    DEFAULT_LIMIT, FindDoc, FindItems, FindResult, GetDoc, GetItem, GetResult, LogDoc, LogResult, MAX_LIMIT, Near,
    NearId, Order, QueryDoc, QueryResult, Target, View,
};
use knowa_model::{BlobView, ClaimView, EntityView, Error, Id, IdKind, Result, Timestamp};

use crate::Sel;
use crate::feed::check_limit;
use crate::merge::{ClaimSort, claim_sort, entity_sort, merge_entities, sort_claims, sort_entities};
use crate::select::{Opened, open_all, select};
use crate::{context, path};

pub(crate) fn query(home: &Home, sel: &Sel, doc: QueryDoc) -> Result<QueryResult> {
    match doc {
        QueryDoc::Find(d) => find(home, sel, d).map(QueryResult::Find),
        QueryDoc::Get(d) => get(home, sel, d).map(QueryResult::Get),
        QueryDoc::Context(d) => context::context(home, sel, d).map(QueryResult::Context),
        QueryDoc::Path(d) => path::path(home, sel, d).map(QueryResult::Path),
        QueryDoc::Log(d) => log(home, sel, d).map(QueryResult::Log),
    }
}

/// `asOf` is reserved (DESIGN §6.1): any non-null value → `unsupported`.
pub(crate) fn reject_as_of(as_of: &Option<serde_json::Value>) -> Result<()> {
    match as_of {
        Some(v) if !v.is_null() => Err(Error::unsupported("asOf (record-time travel) is reserved and not supported in v0")),
        _ => Ok(()),
    }
}

/// `None` → `default`; otherwise `1..=MAX_LIMIT`.
pub(crate) fn limit_or(limit: Option<u32>, default: u32, field: &str) -> Result<u32> {
    let l = limit.unwrap_or(default);
    check_limit(l, field)?;
    Ok(l)
}

/// Checks of a find document that need no space or index.
pub(crate) fn validate_find(d: &FindDoc) -> Result<(u32, Order)> {
    reject_as_of(&d.as_of)?;
    let limit = limit_or(d.limit, DEFAULT_LIMIT, "limit")?;
    // Order docs (query.rs): relevance is the order when none is given.
    let order = d.order.unwrap_or(Order::Relevance);
    let w = &d.where_;
    if w.near.is_some() && w.near_id.is_some() {
        return Err(Error::invalid("where: near and nearId are mutually exclusive"));
    }
    if order == Order::Distance && w.near.is_none() && w.near_id.is_none() {
        return Err(Error::invalid("order: distance requires where.near or where.nearId"));
    }
    Ok((limit, order))
}

/// Multi-space constraints: no cursor (DESIGN §6.1), and an order that can be
/// re-established after merging.
pub(crate) fn validate_multi_find(d: &FindDoc, order: Order) -> Result<()> {
    if d.cursor.is_some() {
        return Err(Error::unsupported("cursor with a multi-space find is not supported in v0; use a larger limit"));
    }
    let has_text = d.where_.text.is_some();
    match d.target {
        Target::Entities => entity_sort(order, has_text).map(|_| ()),
        Target::Claims => claim_sort(order, has_text).map(|_| ()),
    }
}

fn find(home: &Home, sel: &Sel, d: FindDoc) -> Result<FindResult> {
    let (limit, order) = validate_find(&d)?;
    let spaces = select(home, sel, &d.spaces)?;
    let multi = spaces.len() > 1;
    if multi {
        validate_multi_find(&d, order)?;
    }
    let opened = open_all(spaces)?;
    let mut w = d.where_.clone();
    if let Some(nid) = w.near_id.take() {
        w.near = Some(resolve_near_id(&opened, &nid, &d.view)?);
    }
    let has_text = w.text.is_some();
    let cursor = d.cursor.as_deref();
    match d.target {
        Target::Entities => {
            if !multi {
                let page = opened[0].index.find_entities(&w, &d.view, order, limit, cursor)?;
                return Ok(FindResult { items: FindItems::Entities(page.items), cursor: page.cursor });
            }
            let lists = opened
                .iter()
                .map(|o| o.index.find_entities(&w, &d.view, order, limit, None).map(|p| p.items))
                .collect::<Result<Vec<_>>>()?;
            let mut items = sort_entities(merge_entities(lists)?, entity_sort(order, has_text)?)?;
            items.truncate(limit as usize);
            Ok(FindResult { items: FindItems::Entities(items), cursor: None })
        }
        Target::Claims => {
            if !multi {
                let page = opened[0].index.find_claims(&w, &d.view, order, limit, cursor)?;
                return Ok(FindResult { items: FindItems::Claims(page.items), cursor: page.cursor });
            }
            let mut items = Vec::new();
            for o in &opened {
                items.extend(o.index.find_claims(&w, &d.view, order, limit, None)?.items);
            }
            sort_claims(&mut items, claim_sort(order, has_text)?);
            items.truncate(limit as usize);
            Ok(FindResult { items: FindItems::Claims(items), cursor: None })
        }
    }
}

/// `nearId` → `near` at the anchor's display `where` (first selected space that has
/// one), under the query's own view: views apply to evidence (DESIGN §4.7), so a
/// location only an excluded actor asserted must not place the anchor.
fn resolve_near_id(opened: &[Opened], nid: &NearId, view: &View) -> Result<Near> {
    for o in opened {
        if let Some(e) = o.index.entity(&nid.id, view)?
            && let Some(w) = e.where_
        {
            return Ok(Near { lat: w.lat.to_string(), lon: w.lon.to_string(), km: nid.km.clone() });
        }
    }
    Err(Error::invalid(format!("where.nearId: {} has no knowa:where in the selected spaces", nid.id)))
}

fn get(home: &Home, sel: &Sel, d: GetDoc) -> Result<GetResult> {
    reject_as_of(&d.as_of)?;
    if d.ids.is_empty() {
        return Err(Error::invalid("get: ids must not be empty"));
    }
    if let Some(p) = d.ids.iter().find(|i| i.kind() == IdKind::Placeholder) {
        return Err(Error::invalid(format!("get: placeholder {p} is only meaningful inside a write batch")));
    }
    let opened = open_all(select(home, sel, &d.spaces)?)?;
    let mut items = Vec::with_capacity(d.ids.len());
    let mut missing = Vec::new();
    for id in &d.ids {
        let item = match id.kind() {
            IdKind::Claim => find_claim(&opened, id)?.map(|claim| GetItem::Claim { claim }),
            // A blob described by claims (extractor output, a fragment region) reads as
            // an entity, whose claims include its media type and size; bytes nobody
            // has described yet read as a blob.
            IdKind::Blob => match entity_item(&opened, id, &d.view)? {
                Some(item) => Some(item),
                None => blob_view(&opened, id)?.map(|blob| GetItem::Blob { blob }),
            },
            _ => entity_item(&opened, id, &d.view)?,
        };
        match item {
            Some(i) => items.push(i),
            None => missing.push(id.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(Error::not_found(format!(
            "get: {} of {} ids not found in the selected spaces under the given view",
            missing.len(),
            d.ids.len()
        ))
        .with_detail(serde_json::json!({ "missing": missing })));
    }
    Ok(GetResult { items })
}

/// Explicitly requested claims are returned whatever the view, with their status.
fn find_claim(opened: &[Opened], id: &Id) -> Result<Option<ClaimView>> {
    for o in opened {
        if let Some(c) = o.index.claims(std::slice::from_ref(id))?.into_iter().next() {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

/// Merged entity view across the selected spaces, or `None` if no space has it.
pub(crate) fn merged_entity(opened: &[Opened], id: &Id, view: &View) -> Result<Option<EntityView>> {
    let mut lists = Vec::new();
    for o in opened {
        if let Some(e) = o.index.entity(id, view)? {
            lists.push(vec![e]);
        }
    }
    Ok(merge_entities(lists)?.into_iter().next())
}

/// `get` has no limit, so an entity's claims are returned in full up to MAX_LIMIT;
/// beyond that the call fails rather than return a silently truncated list.
fn entity_item(opened: &[Opened], id: &Id, view: &View) -> Result<Option<GetItem>> {
    let Some(entity) = merged_entity(opened, id, view)? else { return Ok(None) };
    let mut claims = Vec::new();
    for o in opened {
        // Index limits cap at MAX_LIMIT, so a full page means "possibly more": refuse
        // rather than return a silently truncated entity (exactly MAX_LIMIT claims is
        // refused too — the price of not having a count).
        claims.extend(o.index.claims_about(id, view, MAX_LIMIT)?.0);
        if claims.len() >= MAX_LIMIT as usize {
            return Err(Error::unsupported(format!(
                "get: {id} has {MAX_LIMIT} or more claims; page through them with find target: claims, where.s: [id]"
            )));
        }
    }
    sort_claims(&mut claims, ClaimSort::RecordedAsc);
    Ok(Some(GetItem::Entity { entity, claims }))
}

fn blob_view(opened: &[Opened], id: &Id) -> Result<Option<BlobView>> {
    let base = id.base();
    let mut view: Option<BlobView> = None;
    for o in opened {
        if !o.space.replica.has_blob(&base) {
            continue;
        }
        match &mut view {
            Some(v) => v.spaces.push(o.space.id().clone()),
            None => {
                let size = fs::metadata(o.space.replica.blob_path(&base)?)?.len();
                view = Some(BlobView { id: id.clone(), size, media_type: None, spaces: vec![o.space.id().clone()] });
            }
        }
    }
    if let Some(v) = &mut view {
        for o in opened {
            if let Some(mt) = o.index.media_type(&base)? {
                v.media_type = Some(mt);
                break;
            }
        }
    }
    Ok(view)
}

fn log(home: &Home, sel: &Sel, d: LogDoc) -> Result<LogResult> {
    let limit = limit_or(d.limit, DEFAULT_LIMIT, "limit")?;
    let since = d
        .since
        .as_deref()
        .map(|s| Timestamp::parse_rfc3339(s).map(|t| t.0).map_err(|e| Error::invalid(format!("since: {}", e.message))))
        .transpose()?;
    let spaces = select(home, sel, &d.spaces)?;
    if spaces.len() > 1 && d.cursor.is_some() {
        return Err(Error::unsupported("cursor with a multi-space log is not supported in v0; use a larger limit"));
    }
    let opened = open_all(spaces)?;
    if let [only] = opened.as_slice() {
        let page = only.index.log(&d.actors, since, limit, d.cursor.as_deref())?;
        return Ok(LogResult { items: page.items, cursor: page.cursor });
    }
    let mut items = Vec::new();
    for o in &opened {
        items.extend(o.index.log(&d.actors, since, limit, None)?.items);
    }
    sort_claims(&mut items, ClaimSort::RecordedAsc);
    items.truncate(limit as usize);
    Ok(LogResult { items, cursor: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::ErrorCode;

    fn find_doc(v: serde_json::Value) -> FindDoc {
        match serde_json::from_value::<QueryDoc>(v).unwrap() {
            QueryDoc::Find(d) => d,
            other => panic!("not a find doc: {other:?}"),
        }
    }

    #[test]
    fn as_of_null_is_allowed_anything_else_unsupported() {
        assert!(reject_as_of(&None).is_ok());
        assert!(reject_as_of(&Some(serde_json::Value::Null)).is_ok());
        let e = reject_as_of(&Some(serde_json::json!("2026-01-01"))).unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
    }

    #[test]
    fn as_of_in_a_document_is_unsupported() {
        let d = find_doc(serde_json::json!({"op": "find", "asOf": "2026-01-01T00:00:00Z"}));
        assert_eq!(validate_find(&d).unwrap_err().code, ErrorCode::Unsupported);
    }

    #[test]
    fn limit_defaults_and_bounds() {
        assert_eq!(limit_or(None, DEFAULT_LIMIT, "limit").unwrap(), DEFAULT_LIMIT);
        assert_eq!(limit_or(Some(7), DEFAULT_LIMIT, "limit").unwrap(), 7);
        assert_eq!(limit_or(Some(0), DEFAULT_LIMIT, "limit").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(limit_or(Some(MAX_LIMIT + 1), DEFAULT_LIMIT, "limit").unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn default_order_is_relevance() {
        let d = find_doc(serde_json::json!({"op": "find"}));
        assert_eq!(validate_find(&d).unwrap(), (DEFAULT_LIMIT, Order::Relevance));
    }

    #[test]
    fn distance_without_near_is_invalid() {
        let d = find_doc(serde_json::json!({"op": "find", "order": "distance"}));
        assert_eq!(validate_find(&d).unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn distance_with_near_id_is_valid() {
        let d = find_doc(serde_json::json!({
            "op": "find", "order": "distance",
            "where": {"nearId": {"id": "e_01arz3ndektsv4rrffq69g5fav", "km": "2"}}
        }));
        assert_eq!(validate_find(&d).unwrap().1, Order::Distance);
    }

    #[test]
    fn near_and_near_id_together_are_invalid() {
        let d = find_doc(serde_json::json!({
            "op": "find",
            "where": {"near": {"lat": "1", "lon": "2", "km": "3"}, "nearId": {"id": "e_01arz3ndektsv4rrffq69g5fav", "km": "2"}}
        }));
        assert_eq!(validate_find(&d).unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn multi_space_cursor_is_unsupported() {
        let d = find_doc(serde_json::json!({"op": "find", "spaces": ["*"], "cursor": "abc"}));
        assert_eq!(validate_multi_find(&d, Order::Relevance).unwrap_err().code, ErrorCode::Unsupported);
    }

    #[test]
    fn multi_space_unmergeable_order_is_unsupported() {
        let d = find_doc(serde_json::json!({"op": "find", "spaces": ["a", "b"], "order": "recorded"}));
        assert_eq!(validate_multi_find(&d, Order::Recorded).unwrap_err().code, ErrorCode::Unsupported);
        let d = find_doc(serde_json::json!({"op": "find", "spaces": ["a", "b"], "target": "claims", "order": "-recorded"}));
        assert!(validate_multi_find(&d, Order::RecordedDesc).is_ok());
    }
}
