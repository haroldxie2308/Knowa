//! Incremental indexing. Applying claims in any order and in any batching yields
//! the same index as one rebuild, because every derived value is a function of the
//! claim set and every value whose inputs changed is recomputed:
//!
//! 1. New claims are inserted with a placeholder status.
//! 2. If a policy input arrived (role, autoAccept, actorKind, retract), the policy
//!    state is recomputed from scratch (it is small); every actor whose owner /
//!    member / autoAccept flags changed has all its claims, and the targets of its
//!    reviews and retracts, queued for status.
//! 3. Status worklist. A claim's (base, status) pair depends only on claims that
//!    reference it (retracts, reviews, replacements), which are newer by
//!    construction: ids are content hashes, so the dependency graph is acyclic and
//!    chaotic iteration converges. When either changes, everything that reads it is
//!    queued.
//! 4. Stale worklist over `src` edges, after statuses have settled.
//! 5. Full-text documents of claims linking a blob whose text-ness may have changed
//!    (other claims' documents never change after insert), then the stored
//!    default-view summary and geo point of every subject whose claims changed.

use std::collections::{BTreeSet, HashSet, VecDeque};

use knowa_model::{Claim, Error, Id, IdKind, Result, Status, vocab};
use rusqlite::{Connection, OptionalExtension};

use crate::blobs::BlobSource;
use crate::db::{canonical, conf_key, db, i64_of, id_from, status_from, value_from};
use crate::entity;
use crate::policy::{self, Acceptance};
use crate::rules;

/// Predicates whose arrival can change the policy state.
const POLICY_INPUTS: [&str; 4] = [vocab::ROLE, vocab::AUTO_ACCEPT, vocab::ACTOR_KIND, vocab::RETRACT];

pub(crate) fn apply(conn: &Connection, space: &Id, genesis_owner: &Id, blobs: &dyn BlobSource, claims: &[Claim]) -> Result<()> {
    let mut work = Work::default();
    let mut next_n: i64 = conn.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM claims", [], |r| r.get(0)).map_err(db)?;
    let mut policy_input = false;
    let mut seen: HashSet<&Id> = HashSet::new();
    for c in claims {
        if c.body.space != *space {
            return Err(Error::invalid(format!(
                "claim {} belongs to space {}, this index is for {}",
                c.id, c.body.space, space
            )));
        }
        // A claim already indexed (or repeated in this batch) is skipped: the index is
        // a function of the claim set, so applying it again changes nothing.
        if !seen.insert(&c.id) || is_indexed(conn, &c.id)? {
            continue;
        }
        if let Some(blob) = insert(conn, c, next_n)? {
            work.blobs.insert(blob);
        }
        next_n += 1;
        policy_input |= POLICY_INPUTS.contains(&c.body.p.as_str());
        work.arrived(conn, c)?;
    }
    if policy_input {
        let old = policy::load(conn)?;
        let new = policy::compute(conn, genesis_owner.as_str())?;
        if old != new {
            for actor in policy::acceptance_changes(&old, &new) {
                work.actor_changed(conn, &actor)?;
            }
            policy::store(conn, &new)?;
        }
    }
    let acc = Acceptance::of(&policy::load(conn)?);
    work.settle_status(conn, &acc)?;
    work.settle_stale(conn)?;
    work.refresh_entities(conn, blobs)
}

/// The policy state of an index without claims: the genesis owner only.
pub(crate) fn init_policy(conn: &Connection, genesis_owner: &Id) -> Result<()> {
    let state = policy::compute(conn, genesis_owner.as_str())?;
    policy::store(conn, &state)
}

fn is_indexed(conn: &Connection, id: &Id) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM claims WHERE id = ?1")
        .map_err(db)?
        .query_row([id.as_str()], |_| Ok(()))
        .optional()
        .map_err(db)?
        .is_some())
}

/// Inserts `c` with arrival number `n`. Returns the blob whose text `c`'s full-text
/// document depends on: that document is written once statuses have settled.
fn insert(conn: &Connection, c: &Claim, n: i64) -> Result<Option<String>> {
    let b = &c.body;
    let p = b.p.as_str();
    let policy = rules::is_policy(p);
    let ent = materializes(c);
    let conf = match &b.conf {
        Some(d) => conf_key(d.as_str())?,
        // DESIGN §6.1: a claim without `conf` counts as 1.
        None => "1".to_string(),
    };
    // placeholder, replaced by the status worklist in this same transaction
    let placeholder = Status::Proposed.as_str();
    conn.prepare_cached(
        "INSERT INTO claims(n, id, actor, seq, at, s, p, policy, ent, o, o_ref_base, conf, equiv, worker, json, base, status, stale)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16, 0)",
    )
    .map_err(db)?
    .execute(rusqlite::params![
        n,
        c.id.as_str(),
        b.actor.as_str(),
        i64_of(b.seq, "seq")?,
        b.at.0,
        b.s.as_str(),
        p,
        policy,
        ent,
        canonical(&b.o)?,
        b.o.as_ref_id().map(|r| r.base().as_str().to_string()),
        conf,
        b.equiv_key()?,
        b.via.get("worker"),
        c.to_json().to_string(),
        placeholder,
    ])
    .map_err(db)?;

    let mut src = conn.prepare_cached("INSERT OR IGNORE INTO src(claim, target) VALUES (?1, ?2)").map_err(db)?;
    for x in b.src.iter().filter(|x| x.kind() == IdKind::Claim) {
        src.execute([c.id.as_str(), x.as_str()]).map_err(db)?;
    }
    let mut rep = conn.prepare_cached("INSERT OR IGNORE INTO replaces(claim, target) VALUES (?1, ?2)").map_err(db)?;
    for x in &b.replaces {
        rep.execute([c.id.as_str(), x.as_str()]).map_err(db)?;
    }
    let q_refs = b.q.values().flatten().filter_map(|v| v.as_ref_id());
    let refs = std::iter::once(&b.s).chain(b.o.as_ref_id()).chain(b.src.iter()).chain(q_refs);
    let mut blob = conn.prepare_cached("INSERT OR IGNORE INTO blob_refs(claim, blob) VALUES (?1, ?2)").map_err(db)?;
    for r in refs.filter(|r| r.kind() == IdKind::Blob) {
        blob.execute([c.id.as_str(), r.base().as_str()]).map_err(db)?;
    }

    if !ent {
        return Ok(None);
    }
    match entity::linked_blob(&b.s, p, &b.o) {
        Some(blob) => Ok(Some(blob.as_str().to_string())),
        None => {
            entity::insert_text(conn, n, p, &b.o)?;
            Ok(None)
        }
    }
}

#[derive(Default)]
struct Queue {
    q: VecDeque<String>,
    queued: HashSet<String>,
}

impl Queue {
    fn push(&mut self, id: &str) {
        if self.queued.insert(id.to_string()) {
            self.q.push_back(id.to_string());
        }
    }
    fn pop(&mut self) -> Option<String> {
        let id = self.q.pop_front()?;
        self.queued.remove(&id);
        Some(id)
    }
}

#[derive(Default)]
struct Work {
    status: Queue,
    stale: Queue,
    entities: BTreeSet<String>,
    /// bare blobs whose linking claims need their full-text documents (re)written
    blobs: BTreeSet<String>,
}

struct ClaimState {
    actor: String,
    p: String,
    s: String,
    ent: bool,
    base: Status,
    status: Status,
}

fn claim_state(conn: &Connection, id: &str) -> Result<Option<ClaimState>> {
    let row = conn
        .prepare_cached("SELECT actor, p, s, ent, base, status FROM claims WHERE id = ?1")
        .map_err(db)?
        .query_row([id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, bool>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
        })
        .optional()
        .map_err(db)?;
    match row {
        None => Ok(None),
        Some((actor, p, s, ent, base, status)) => {
            Ok(Some(ClaimState { actor, p, s, ent, base: status_from(&base)?, status: status_from(&status)? }))
        }
    }
}

/// DESIGN §4.7 / vocab: subjects of non-policy claims about non-claim ids are entities.
fn materializes(c: &Claim) -> bool {
    !rules::is_policy(c.body.p.as_str()) && c.body.s.kind() != IdKind::Claim
}

fn is_targeting(p: &str) -> bool {
    p == vocab::REVIEW || p == vocab::RETRACT
}

/// Ids of claims selected by `sql` with one `?1` parameter.
fn ids(conn: &Connection, sql: &str, param: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(sql).map_err(db)?;
    let mut rows = stmt.query([param]).map_err(db)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db)? {
        out.push(row.get(0).map_err(db)?);
    }
    Ok(out)
}

/// Claims listing `id` in `src`.
fn dependents(conn: &Connection, id: &str) -> Result<Vec<String>> {
    ids(conn, "SELECT claim FROM src WHERE target = ?1", id)
}

impl Work {
    fn arrived(&mut self, conn: &Connection, c: &Claim) -> Result<()> {
        let id = c.id.as_str();
        self.status.push(id);
        self.stale.push(id);
        for t in &c.body.replaces {
            self.status.push(t.as_str());
        }
        if is_targeting(c.body.p.as_str()) {
            self.status.push(c.body.s.as_str());
        }
        // claims that arrived earlier and cite this one
        for d in dependents(conn, id)? {
            self.stale.push(&d);
        }
        if materializes(c) {
            self.entities.insert(c.body.s.as_str().to_string());
        }
        Ok(())
    }

    /// The actor's acceptance (rule 4) and the effect of its reviews/retracts changed.
    fn actor_changed(&mut self, conn: &Connection, actor: &str) -> Result<()> {
        let mut stmt = conn.prepare_cached("SELECT id, p, s FROM claims WHERE actor = ?1").map_err(db)?;
        let mut rows = stmt.query([actor]).map_err(db)?;
        while let Some(row) = rows.next().map_err(db)? {
            let id: String = row.get(0).map_err(db)?;
            let p: String = row.get(1).map_err(db)?;
            self.status.push(&id);
            if is_targeting(&p) {
                self.status.push(&row.get::<_, String>(2).map_err(db)?);
            }
        }
        Ok(())
    }

    fn settle_status(&mut self, conn: &Connection, acc: &Acceptance) -> Result<()> {
        while let Some(id) = self.status.pop() {
            // Targets of replaces/reviews/retracts may not have arrived (yet).
            let Some(cur) = claim_state(conn, &id)? else { continue };
            let (base, status) = status_of(conn, acc, &id, &cur.actor)?;
            if (base, status) == (cur.base, cur.status) {
                continue;
            }
            conn.prepare_cached("UPDATE claims SET base = ?1, status = ?2 WHERE id = ?3")
                .map_err(db)?
                .execute([base.as_str(), status.as_str(), id.as_str()])
                .map_err(db)?;
            // the targets of a replacement read its base status (rule 2)
            if base != cur.base {
                for t in ids(conn, "SELECT target FROM replaces WHERE claim = ?1", &id)? {
                    self.status.push(&t);
                }
            }
            if status != cur.status {
                // a review/retract's effect reads whether it is retracted
                if is_targeting(&cur.p) {
                    self.status.push(&cur.s);
                }
                for d in dependents(conn, &id)? {
                    self.stale.push(&d);
                }
                if cur.ent {
                    self.entities.insert(cur.s);
                }
            }
        }
        Ok(())
    }

    fn settle_stale(&mut self, conn: &Connection) -> Result<()> {
        while let Some(id) = self.stale.pop() {
            let cur: Option<bool> = conn
                .prepare_cached("SELECT stale FROM claims WHERE id = ?1")
                .map_err(db)?
                .query_row([id.as_str()], |r| r.get(0))
                .optional()
                .map_err(db)?;
            let Some(cur) = cur else { continue };
            let new = stale_of(conn, &id)?;
            if new == cur {
                continue;
            }
            conn.prepare_cached("UPDATE claims SET stale = ?1 WHERE id = ?2")
                .map_err(db)?
                .execute(rusqlite::params![new, id])
                .map_err(db)?;
            for d in dependents(conn, &id)? {
                self.stale.push(&d);
            }
        }
        Ok(())
    }

    fn refresh_entities(&self, conn: &Connection, blobs: &dyn BlobSource) -> Result<()> {
        let mut text_blobs = self.blobs.clone();
        for e in &self.entities {
            // A bare blob's claims changed, so its media type may have: that decides
            // whether the claims linking it carry its text.
            let id = id_from(e)?;
            if id.kind() == IdKind::Blob && id.fragment().is_none() {
                text_blobs.insert(e.clone());
            }
        }
        for b in &text_blobs {
            entity::refresh_blob_text(conn, blobs, b)?;
        }
        for e in &self.entities {
            entity::refresh(conn, e)?;
        }
        Ok(())
    }
}

/// Gathers the inputs of DESIGN §4.7 for claim `id` by `author` and decides its
/// (base status, status).
fn status_of(conn: &Connection, acc: &Acceptance, id: &str, author: &str) -> Result<(Status, Status)> {
    let mut retracted = false;
    let mut review: Option<(i64, String, rules::Verdict)> = None;
    let mut stmt = conn
        .prepare_cached("SELECT id, actor, p, o, at, status FROM claims WHERE s = ?1 AND p IN (?2, ?3)")
        .map_err(db)?;
    let mut rows = stmt.query([id, vocab::RETRACT, vocab::REVIEW]).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        let rid: String = row.get(0).map_err(db)?;
        let ractor: String = row.get(1).map_err(db)?;
        let p: String = row.get(2).map_err(db)?;
        let o = value_from(&row.get::<_, String>(3).map_err(db)?)?;
        let at: i64 = row.get(4).map_err(db)?;
        let r_retracted = status_from(&row.get::<_, String>(5).map_err(db)?)? == Status::Retracted;
        if p == vocab::RETRACT {
            retracted |= rules::retract_effective(&o, ractor == author, acc.is_owner(&ractor), r_retracted);
        } else if rules::review_effective(acc.is_reviewer(&ractor), r_retracted)
            && let Some(v) = rules::verdict(&o)
            && review.as_ref().is_none_or(|(a, i, _)| (at, &rid) > (*a, i))
        {
            review = Some((at, rid, v));
        }
    }
    let base = rules::base_status(retracted, review.map(|(_, _, v)| v), acc.author_accepted(author));
    let mut superseded = false;
    let mut stmt = conn
        .prepare_cached("SELECT c.base FROM replaces r JOIN claims c ON c.id = r.claim WHERE r.target = ?1")
        .map_err(db)?;
    let mut rows = stmt.query([id]).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        superseded |= rules::supersedes(status_from(&row.get::<_, String>(0).map_err(db)?)?);
    }
    Ok((base, rules::status(base, superseded)))
}

fn stale_of(conn: &Connection, id: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare_cached("SELECT c.status, c.stale FROM src s JOIN claims c ON c.id = s.target WHERE s.claim = ?1")
        .map_err(db)?;
    let mut rows = stmt.query([id]).map_err(db)?;
    let mut stale = false;
    while let Some(row) = rows.next().map_err(db)? {
        let status = status_from(&row.get::<_, String>(0).map_err(db)?)?;
        stale |= rules::makes_stale(status, row.get(1).map_err(db)?);
    }
    Ok(stale)
}
