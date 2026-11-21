//! `review` sugar (DESIGN §7): one `knowa:review` per target claim, all in one write.

use std::collections::{BTreeMap, HashSet};

use knowa_model::query::{FindDoc, FindItems, MAX_LIMIT, Order, QueryDoc, QueryResult, Target, View, Where, WriteResult};
use knowa_model::{Draft, Error, Id, IdKind, Result, Status, Value, vocab};

use crate::write::term;
use crate::{Knowa, ReviewDecision, Sel};

pub(crate) fn review(
    k: &Knowa,
    sel: &Sel,
    claims: &[Id],
    select: Option<Where>,
    decision: ReviewDecision,
    note: Option<String>,
) -> Result<WriteResult> {
    if claims.is_empty() && select.is_none() {
        return Err(Error::usage(
            "review: name the claims to review or a selector (--by, --about, --p, --since, --until); there is no review of everything",
        ));
    }
    if select.as_ref().is_some_and(|w| *w == Where::default()) {
        return Err(Error::usage("review: an empty selector would select every proposed claim; set at least one of its fields"));
    }
    if let Some(c) = claims.iter().find(|c| c.kind() != IdKind::Claim) {
        return Err(Error::invalid(format!("review: {c} is not a claim id (c_…)")));
    }
    let mut targets = claims.to_vec();
    if let Some(w) = select {
        targets.extend(proposed(k, sel, w)?);
    }
    let mut seen = HashSet::new();
    targets.retain(|t| seen.insert(t.clone()));
    if targets.is_empty() {
        return Ok(WriteResult { items: Vec::new(), placeholders: BTreeMap::new() });
    }
    let p = term(vocab::REVIEW)?;
    let o = Value::Term(term(decision.term())?);
    let drafts = targets
        .into_iter()
        .map(|t| {
            let mut d = Draft::new(t, p.clone(), o.clone());
            d.note = note.clone();
            d
        })
        .collect();
    k.write(sel, drafts)
}

/// Ids of every proposed claim in the caller's space (the space the write goes to)
/// matching `w`, oldest first.
fn proposed(k: &Knowa, sel: &Sel, w: Where) -> Result<Vec<Id>> {
    let mut doc = FindDoc {
        spaces: Vec::new(),
        view: View { status: vec![Status::Proposed], ..View::default() },
        as_of: None,
        target: Target::Claims,
        where_: w,
        order: Some(Order::Recorded),
        limit: Some(MAX_LIMIT),
        cursor: None,
    };
    let mut out = Vec::new();
    loop {
        let page = match k.query(sel, QueryDoc::Find(doc.clone()))? {
            QueryResult::Find(f) => f,
            _ => return Err(Error::internal("review: a find query returned another op's result")),
        };
        match page.items {
            FindItems::Claims(cs) => out.extend(cs.into_iter().map(|c| c.claim.id)),
            FindItems::Entities(_) => return Err(Error::internal("review: a claims find returned entities")),
        }
        match page.cursor {
            Some(c) => doc.cursor = Some(c),
            None => return Ok(out),
        }
    }
}
