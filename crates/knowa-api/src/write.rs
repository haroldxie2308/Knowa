//! The write primitive (DESIGN §4.4, §5.3 "write path", §6).
//!
//! `Txn` holds the locks of the actors it writes as and their index. Every claim the
//! api appends — `Knowa::write`, ingest, extractors, workers, `init` — goes through
//! `Txn::write`, so placeholder minting, the actorKind prelude, idempotency and
//! chaining have exactly one implementation.

use std::collections::{BTreeMap, HashMap};

use knowa_config::Actor;
use knowa_index::Index;
use knowa_model::query::{Order, View, Where, WriteItem, WriteResult};
use knowa_model::{ClaimBody, Draft, Error, ErrorCode, Id, Result, Status, Term, Timestamp, Value, vocab};
use knowa_store::ActorLock;

use crate::select::Space;

/// Attempts at one write, in all, where another writer of the actor can append first
/// (an object store, DESIGN §5.3): the loser redoes its write on the new head.
pub(crate) const WRITE_ATTEMPTS: u32 = 3;

/// One write call's outcome, plus what composite ops (`init`, `put`, workers) need.
pub(crate) struct Written {
    pub result: WriteResult,
    /// id of the `knowa:actorKind` claim prepended by this call, if one was
    pub prelude: Option<Id>,
    /// number of the caller's drafts that produced a new claim
    pub created: u64,
}

/// How one attempt at a write ended.
enum Attempt {
    /// the batch is appended and applied: the items for the caller's drafts, and the
    /// actorKind prelude's id if one was written
    Done { items: Vec<WriteItem>, prelude: Option<Id>, created: u64 },
    /// another writer of the actor appended first; nothing of this attempt was written
    Lost(Error),
}

pub(crate) struct Txn<'a> {
    space: &'a Space,
    /// ascending by actor id, one per actor
    locks: Vec<ActorLock>,
    index: Index,
}

impl<'a> Txn<'a> {
    /// Refuses readonly spaces, then follows DESIGN §5.3:
    /// - opens the index, rebuilding it if it must be, before any actor lock is
    ///   taken: a rebuild reads every actor's log and may wait for other writers,
    ///   which a writer holding its own actor lock must never do;
    /// - takes the locks of `actors` in ascending id order, the one order every
    ///   writer uses, so two writers cannot wait for each other.
    ///
    /// Each `write` catches the index up with its actor's log. The locks are held until
    /// the `Txn` is dropped.
    pub fn begin(space: &'a Space, actors: &[&Actor]) -> Result<Txn<'a>> {
        space.ensure_writable()?;
        let mut ids: Vec<Id> = actors.iter().map(|a| a.id.clone()).collect();
        ids.sort();
        ids.dedup();
        let index = Index::load(&space.replica, &space.index_dir)?;
        let locks = ids.iter().map(|id| space.replica.lock_actor(id)).collect::<Result<Vec<_>>>()?;
        Ok(Txn { space, locks, index })
    }

    /// All-or-nothing: every draft is validated, resolved and sealed before anything
    /// is appended, and the store's append writes nothing on error.
    ///
    /// Where another writer of the actor can append first (`Replica::writers_can_race`:
    /// an object store), an append that loses with `conflict` is redone (DESIGN §5.3):
    /// the head is read again, the index caught up with it, the plan redone (the other
    /// writer may have written an equivalent claim), and the claims rebuilt and signed
    /// again; after `WRITE_ATTEMPTS` attempts in all the `conflict` is returned. A
    /// directory's lock keeps other writers out, so there a `conflict` is returned at
    /// once.
    pub fn write(&mut self, actor: &Actor, drafts: Vec<Draft>) -> Result<Written> {
        if drafts.is_empty() {
            return Err(Error::invalid("write: no drafts"));
        }
        for (i, d) in drafts.iter().enumerate() {
            d.validate().map_err(|e| at_draft(e, i))?;
            // DESIGN §4.6: the prelude below is the only writer of actorKind; an explicit
            // one is an actor relabeling itself (e.g. an agent claiming to be Human).
            if d.p.as_str() == vocab::ACTOR_KIND {
                return Err(at_draft(
                    Error::invalid(format!(
                        "{} is written by the system (an actor's first write in a space declares its kind); it cannot be drafted",
                        vocab::ACTOR_KIND
                    )),
                    i,
                ));
            }
        }
        let minted = mint_placeholders(&drafts)?;
        let resolved = drafts
            .iter()
            .enumerate()
            .map(|(i, d)| d.resolve(&minted).map_err(|e| at_draft(e, i)))
            .collect::<Result<Vec<_>>>()?;

        let mut attempt = 1;
        let (items, prelude, created) = loop {
            match self.attempt(actor, &resolved)? {
                Attempt::Done { items, prelude, created } => break (items, prelude, created),
                Attempt::Lost(_) if attempt < WRITE_ATTEMPTS => attempt += 1,
                Attempt::Lost(mut e) => {
                    e.message = format!("{} (gave up after {WRITE_ATTEMPTS} attempts)", e.message);
                    return Err(e);
                }
            }
        };
        let placeholders = minted.into_iter().map(|(p, e)| (p.as_str().to_string(), e)).collect();
        Ok(Written { result: WriteResult { items, placeholders }, prelude, created })
    }

    /// One attempt at `write`'s resolved drafts, on the actor's head as it is now
    /// (DESIGN §5.3 write path, steps 2–6).
    fn attempt(&mut self, actor: &Actor, resolved: &[Draft]) -> Result<Attempt> {
        let space = self.space;
        let lock = lock_of(&self.locks, actor)?;
        // The head first, then the index up to it. The claims chain onto this head, so
        // the index must hold every claim up to it before they are applied over them:
        // catch-up resumes above the highest indexed seq and would never see a claim
        // left below. Under a directory's lock nothing moves and the catch-up finds
        // nothing new. On an object store another writer may append after the head was
        // read; then the append below loses with `conflict`.
        let head = space.replica.head(lock)?;
        self.index.catch_up_actors(&space.replica, std::slice::from_ref(&actor.id))?;

        // Prelude (DESIGN §4.6): an actor's kind is self-declared by its first claim in
        // the space, prepended to the batch.
        let needs_prelude = !self.has_declared_kind(actor)?;
        let mut entries = Vec::with_capacity(resolved.len() + 1);
        if needs_prelude {
            entries.push(actor_kind_draft(actor)?);
        }
        let first_user = entries.len();
        entries.extend(resolved.iter().cloned());

        let keys = entries.iter().map(Draft::equiv_key).collect::<Result<Vec<_>>>()?;
        let index = &self.index;
        let plan = plan_batch(&keys, |k| Ok(index.equivalent(&actor.id, k)?.map(|cv| cv.claim.id)))?;

        let space_id = space.id().clone();
        let (mut seq, mut prev) = match head {
            Some(h) => (h.seq, Some(h.id)),
            None => (0, None),
        };
        let at = Timestamp::now();
        let mut ids: Vec<Id> = Vec::with_capacity(entries.len());
        let mut claims = Vec::new();
        for (draft, step) in entries.into_iter().zip(&plan) {
            let id = match step {
                Plan::Existing(id) => id.clone(),
                // `plan_batch` only points backwards, so `ids[*j]` is already filled.
                Plan::SameAs(j) => ids[*j].clone(),
                Plan::New => {
                    seq += 1;
                    let body = ClaimBody::from_draft(draft, space_id.clone(), actor.id.clone(), seq, prev.take(), at);
                    let claim = actor.signer.seal(body)?;
                    prev = Some(claim.id.clone());
                    let id = claim.id.clone();
                    claims.push(claim);
                    id
                }
            };
            ids.push(id);
        }

        if !claims.is_empty() {
            match space.replica.append(lock, &claims) {
                Ok(()) => {}
                // Another writer of the actor appended first, which only a store without
                // locks lets happen. Nothing of this batch was written.
                Err(e) if e.code == ErrorCode::Conflict && space.replica.writers_can_race() => {
                    return Ok(Attempt::Lost(e));
                }
                Err(e) => return Err(e),
            }
            // From here the claims are durable. If apply fails the error propagates and
            // the next catch-up applies them from the log (DESIGN §5.4).
            self.index.apply(&space.replica, &claims)?;
        }

        let user_ids = &ids[first_user..];
        let statuses: HashMap<Id, _> =
            self.index.claims(user_ids)?.into_iter().map(|cv| (cv.claim.id.clone(), cv.status)).collect();
        let mut items = Vec::with_capacity(user_ids.len());
        let mut created = 0;
        for (id, step) in user_ids.iter().zip(&plan[first_user..]) {
            let status = *statuses.get(id).ok_or_else(|| {
                Error::internal(format!("write: index has no status for claim {id} it just applied or matched"))
            })?;
            let is_new = matches!(step, Plan::New);
            created += u64::from(is_new);
            items.push(WriteItem { id: id.clone(), created: is_new, status });
        }
        Ok(Attempt::Done { items, prelude: needs_prelude.then(|| ids[0].clone()), created })
    }

    /// The bytes of `blob` were just stored. Claims about it may already be indexed
    /// (a catalog-only put, a partial import) and their full-text documents lack its
    /// text; no claim arrives to make the index revisit them (DESIGN §7.2).
    pub fn refresh_blob(&mut self, blob: &Id) -> Result<()> {
        self.index.refresh_blob(&self.space.replica, blob)
    }
}

impl Txn<'_> {
    /// Whether the actor already has any `knowa:actorKind` claim here, whatever its
    /// status: only the first one is effective (DESIGN §4.6), so a rejected or
    /// retracted first declaration must not be followed by another.
    fn has_declared_kind(&self, actor: &Actor) -> Result<bool> {
        let w = Where {
            s: vec![actor.id.clone()],
            p: vec![term(vocab::ACTOR_KIND)?],
            actors: vec![actor.id.clone()],
            ..Where::default()
        };
        let every_status = View {
            status: vec![Status::Accepted, Status::Proposed, Status::Rejected, Status::Retracted, Status::Superseded],
            ..View::default()
        };
        Ok(!self.index.find_claims(&w, &every_status, Order::Recorded, 1, None)?.items.is_empty())
    }
}

/// The lock a transaction holds for `actor`; an actor it did not lock is a caller bug.
fn lock_of<'l>(locks: &'l [ActorLock], actor: &Actor) -> Result<&'l ActorLock> {
    locks
        .iter()
        .find(|l| *l.actor() == actor.id)
        .ok_or_else(|| Error::internal(format!("write as {} in a transaction that did not lock it", actor.id)))
}

/// What to do with one batch entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plan {
    /// an equivalent active (accepted|proposed), non-stale claim by the same actor exists: no-op
    Existing(Id),
    /// duplicate of an earlier entry in this batch (index into the batch)
    SameAs(usize),
    /// append a new claim
    New,
}

/// Dedupe (DESIGN §4.4): within the batch first (the second identical draft points at
/// the first), then against the index via `existing(equiv_key)`.
pub(crate) fn plan_batch(keys: &[String], mut existing: impl FnMut(&str) -> Result<Option<Id>>) -> Result<Vec<Plan>> {
    let mut first: HashMap<&str, usize> = HashMap::new();
    let mut plan = Vec::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        if let Some(&j) = first.get(k.as_str()) {
            plan.push(Plan::SameAs(j));
            continue;
        }
        first.insert(k, i);
        plan.push(match existing(k)? {
            Some(id) => Plan::Existing(id),
            None => Plan::New,
        });
    }
    Ok(plan)
}

/// Each distinct `_:x` across the batch → one fresh entity id.
pub(crate) fn mint_placeholders(drafts: &[Draft]) -> Result<BTreeMap<Id, Id>> {
    let mut map = BTreeMap::new();
    for d in drafts {
        for p in d.placeholders() {
            if let std::collections::btree_map::Entry::Vacant(slot) = map.entry(p) {
                slot.insert(Id::new_entity()?);
            }
        }
    }
    Ok(map)
}

pub(crate) fn term(s: &str) -> Result<Term> {
    Term::parse(s)
}


fn actor_kind_draft(actor: &Actor) -> Result<Draft> {
    Ok(Draft::new(actor.id.clone(), term(vocab::ACTOR_KIND)?, Value::Term(term(actor.kind.as_term())?)))
}

/// Sets `via.worker` on a worker/extractor draft. A draft that already names a
/// different worker → `invalid` (a worker may not write as another).
pub(crate) fn stamp_worker(d: &mut Draft, worker: &str) -> Result<()> {
    match d.via.get("worker") {
        None => {
            d.via.insert("worker".to_string(), worker.to_string());
            Ok(())
        }
        Some(w) if w == worker => Ok(()),
        Some(w) => Err(Error::invalid(format!("draft sets via.worker {w:?}, but it is being written by worker {worker:?}"))),
    }
}

fn at_draft(mut e: Error, i: usize) -> Error {
    e.message = format!("draft {i}: {}", e.message);
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::IdKind;

    fn id(s: &str) -> Id {
        Id::parse(s).unwrap()
    }

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plan_dedupes_within_batch_against_first_occurrence() {
        let plan = plan_batch(&keys(&["k1", "k2", "k1", "k1"]), |_| Ok(None)).unwrap();
        assert_eq!(plan, vec![Plan::New, Plan::New, Plan::SameAs(0), Plan::SameAs(0)]);
    }

    #[test]
    fn plan_reuses_existing_and_points_duplicates_at_it() {
        let existing = id("e_01arz3ndektsv4rrffq69g5fav");
        let plan = plan_batch(&keys(&["old", "new", "old"]), |k| Ok((k == "old").then(|| existing.clone()))).unwrap();
        assert_eq!(plan, vec![Plan::Existing(existing), Plan::New, Plan::SameAs(0)]);
    }

    #[test]
    fn plan_looks_up_each_distinct_key_once() {
        let mut calls = Vec::new();
        plan_batch(&keys(&["a", "a", "b", "a"]), |k| {
            calls.push(k.to_string());
            Ok(None)
        })
        .unwrap();
        assert_eq!(calls, vec!["a", "b"]);
    }

    #[test]
    fn plan_propagates_lookup_errors() {
        let err = plan_batch(&keys(&["a"]), |_| Err(Error::internal("boom"))).unwrap_err();
        assert_eq!(err.message, "boom");
    }

    fn draft(s: &str, o: Value) -> Draft {
        Draft::new(id(s), Term::parse("knowa:about").unwrap(), o)
    }

    #[test]
    fn placeholders_mint_one_entity_per_distinct_name() {
        let drafts = vec![
            draft("_:a", Value::Ref(id("_:b"))),
            draft("_:a", Value::text("x")),
            draft("e_01arz3ndektsv4rrffq69g5fav", Value::Ref(id("_:b"))),
        ];
        let map = mint_placeholders(&drafts).unwrap();
        assert_eq!(map.len(), 2);
        let a = &map[&id("_:a")];
        let b = &map[&id("_:b")];
        assert_ne!(a, b);
        assert_eq!(a.kind(), IdKind::Entity);
        assert_eq!(b.kind(), IdKind::Entity);
        let r0 = drafts[0].resolve(&map).unwrap();
        let r1 = drafts[1].resolve(&map).unwrap();
        assert_eq!(r0.s, *a);
        assert_eq!(r1.s, *a);
        assert_eq!(r0.o, Value::Ref(b.clone()));
    }

    #[test]
    fn no_placeholders_mint_nothing() {
        let drafts = vec![draft("e_01arz3ndektsv4rrffq69g5fav", Value::text("x"))];
        assert!(mint_placeholders(&drafts).unwrap().is_empty());
    }

    #[test]
    fn stamp_worker_sets_missing_and_accepts_same() {
        let mut d = draft("e_01arz3ndektsv4rrffq69g5fav", Value::text("x"));
        stamp_worker(&mut d, "summarizer").unwrap();
        assert_eq!(d.via.get("worker").map(String::as_str), Some("summarizer"));
        stamp_worker(&mut d, "summarizer").unwrap();
    }

    #[test]
    fn stamp_worker_rejects_other_worker() {
        let mut d = draft("e_01arz3ndektsv4rrffq69g5fav", Value::text("x"));
        d.via.insert("worker".into(), "other".into());
        assert_eq!(stamp_worker(&mut d, "summarizer").unwrap_err().code, knowa_model::ErrorCode::Invalid);
    }
}
