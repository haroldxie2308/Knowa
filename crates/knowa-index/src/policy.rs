//! Policy state (DESIGN §4.6): owner set, members, autoAccept, roles and declared
//! actor kinds. A pure function of the `knowa:role` / `knowa:autoAccept` /
//! `knowa:actorKind` claims and the retract chains above them.
//!
//! Owners need care: a role grant counts only if its author is an owner and it is
//! not retracted, and a retract counts only if its author is an owner (or the
//! target's author) and it is not retracted. Owner-ship and retraction therefore
//! depend on each other through negation. We take the well-founded model (Van
//! Gelder's alternating fixpoint): in every replica where the rules have a single
//! consistent answer this is that answer; where owners revoke each other in a
//! cycle, the actors whose ownership is undecidable are not owners. It is a pure
//! function of the claim set, so incremental indexing and rebuild agree.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use knowa_model::{ActorKind, Error, IdKind, Result, Role, Value, vocab};
use rusqlite::Connection;

use crate::db::{db, id_from, value_from};
use crate::rules;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ActorPolicy {
    pub owner: bool,
    pub member: bool,
    pub auto: bool,
    pub role: Option<Role>,
    pub kind: Option<ActorKind>,
}

/// actor id → its policy; actors without any policy fact are absent.
pub(crate) type PolicyState = BTreeMap<String, ActorPolicy>;

/// The parts of the policy state that status computation reads.
pub(crate) struct Acceptance {
    owners: HashSet<String>,
    reviewers: HashSet<String>,
    accepted_authors: HashSet<String>,
}

impl Acceptance {
    pub fn of(state: &PolicyState) -> Acceptance {
        let mut a = Acceptance { owners: HashSet::new(), reviewers: HashSet::new(), accepted_authors: HashSet::new() };
        for (id, p) in state {
            if p.owner {
                a.owners.insert(id.clone());
            }
            if p.owner || p.member {
                a.reviewers.insert(id.clone());
            }
            if p.owner || p.member || p.auto {
                a.accepted_authors.insert(id.clone());
            }
        }
        a
    }
    pub fn is_owner(&self, actor: &str) -> bool {
        self.owners.contains(actor)
    }
    /// owner or member (may review)
    pub fn is_reviewer(&self, actor: &str) -> bool {
        self.reviewers.contains(actor)
    }
    /// owner, member or effective autoAccept (DESIGN §4.7 rule 4)
    pub fn author_accepted(&self, actor: &str) -> bool {
        self.accepted_authors.contains(actor)
    }
}

/// Actors whose status-relevant flags (owner, member, autoAccept) differ.
pub(crate) fn acceptance_changes(old: &PolicyState, new: &PolicyState) -> Vec<String> {
    let none = ActorPolicy::default();
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter(|k| {
            let a = old.get(*k).unwrap_or(&none);
            let b = new.get(*k).unwrap_or(&none);
            (a.owner, a.member, a.auto) != (b.owner, b.member, b.auto)
        })
        .cloned()
        .collect()
}

pub(crate) fn load(conn: &Connection) -> Result<PolicyState> {
    let mut stmt = conn.prepare_cached("SELECT id, kind, role, owner, member, auto FROM actors").map_err(db)?;
    let mut rows = stmt.query([]).map_err(db)?;
    let mut state = PolicyState::new();
    while let Some(row) = rows.next().map_err(db)? {
        let id: String = row.get(0).map_err(db)?;
        let kind: Option<String> = row.get(1).map_err(db)?;
        let role: Option<String> = row.get(2).map_err(db)?;
        let kind = match kind {
            Some(k) => Some(rules::kind_of(&k).ok_or_else(|| crate::db::corrupt(format_args!("actor kind {k:?}")))?),
            None => None,
        };
        let role = match role {
            Some(r) => Some(rules::role_of(&r).ok_or_else(|| crate::db::corrupt(format_args!("actor role {r:?}")))?),
            None => None,
        };
        let p = ActorPolicy {
            owner: row.get::<_, i64>(3).map_err(db)? != 0,
            member: row.get::<_, i64>(4).map_err(db)? != 0,
            auto: row.get::<_, i64>(5).map_err(db)? != 0,
            role,
            kind,
        };
        state.insert(id, p);
    }
    Ok(state)
}

pub(crate) fn store(conn: &Connection, state: &PolicyState) -> Result<()> {
    conn.execute("DELETE FROM actors", []).map_err(db)?;
    let mut stmt = conn
        .prepare_cached("INSERT INTO actors(id, kind, role, owner, member, auto) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
        .map_err(db)?;
    for (id, p) in state {
        stmt.execute(rusqlite::params![
            id,
            p.kind.map(ActorKind::as_term),
            p.role.map(Role::as_term),
            p.owner,
            p.member,
            p.auto
        ])
        .map_err(db)?;
    }
    Ok(())
}

struct PClaim {
    id: String,
    author: String,
    at: i64,
    s: String,
    s_is_actor: bool,
    p: String,
    o: Value,
}

/// Role/autoAccept/actorKind claims plus every retract chain above them.
#[derive(Default)]
struct Graph {
    claims: Vec<PClaim>,
    /// target index → indices of retracts whose `s` is that target. Every retract has
    /// exactly one target, so this is a forest.
    retracts_on: HashMap<usize, Vec<usize>>,
    /// (retract, target)
    retracts: Vec<(usize, usize)>,
    /// role grants of `knowa:Owner` to an actor id
    owner_grants: Vec<usize>,
}

const BASE_PREDICATES: [&str; 3] = [vocab::ROLE, vocab::AUTO_ACCEPT, vocab::ACTOR_KIND];

fn load_graph(conn: &Connection) -> Result<Graph> {
    fn read(row: &rusqlite::Row<'_>) -> Result<PClaim> {
        let s: String = row.get(3).map_err(db)?;
        let o: String = row.get(5).map_err(db)?;
        Ok(PClaim {
            id: row.get(0).map_err(db)?,
            author: row.get(1).map_err(db)?,
            at: row.get(2).map_err(db)?,
            s_is_actor: id_from(&s)?.kind() == IdKind::Actor,
            s,
            p: row.get(4).map_err(db)?,
            o: value_from(&o)?,
        })
    }
    let mut g = Graph::default();
    let mut base = conn
        .prepare_cached("SELECT id, actor, at, s, p, o FROM claims WHERE p IN (?1, ?2, ?3) ORDER BY id")
        .map_err(db)?;
    let mut rows = base.query(BASE_PREDICATES).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        g.claims.push(read(row)?);
    }
    let mut frontier: Vec<usize> = (0..g.claims.len()).collect();
    let mut on = conn
        .prepare_cached("SELECT id, actor, at, s, p, o FROM claims WHERE s = ?1 AND p = ?2 ORDER BY id")
        .map_err(db)?;
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for t in frontier {
            let mut rows = on.query(rusqlite::params![g.claims[t].id, vocab::RETRACT]).map_err(db)?;
            while let Some(row) = rows.next().map_err(db)? {
                let i = g.claims.len();
                g.claims.push(read(row)?);
                g.retracts_on.entry(t).or_default().push(i);
                g.retracts.push((i, t));
                next.push(i);
            }
        }
        frontier = next;
    }
    for (i, c) in g.claims.iter().enumerate() {
        if c.p == vocab::ROLE && c.s_is_actor && matches!(&c.o, Value::Term(t) if t.as_str() == vocab::ROLE_OWNER) {
            g.owner_grants.push(i);
        }
    }
    Ok(g)
}

/// Γ(neg): the least model of the policy rules where "x is not retracted" is read
/// as `!neg[x]`. Returns (retracted, owners). Anti-monotone in `neg`.
fn gamma(g: &Graph, genesis: &str, neg: &[bool]) -> (Vec<bool>, HashSet<String>) {
    let mut owners = HashSet::from([genesis.to_string()]);
    let mut ret = vec![false; g.claims.len()];
    loop {
        let mut changed = false;
        for &i in &g.owner_grants {
            let c = &g.claims[i];
            if !neg[i] && owners.contains(&c.author) && owners.insert(c.s.clone()) {
                changed = true;
            }
        }
        for &(r, t) in &g.retracts {
            let (rc, tc) = (&g.claims[r], &g.claims[t]);
            if !ret[t] && rules::retract_effective(&rc.o, rc.author == tc.author, owners.contains(&rc.author), neg[r]) {
                ret[t] = true;
                changed = true;
            }
        }
        if !changed {
            return (ret, owners);
        }
    }
}

/// Owners in the well-founded model. K grows monotonically (Γ² is monotone), so the
/// loop ends after at most |claims| + 1 rounds.
fn owners(g: &Graph, genesis: &str) -> HashSet<String> {
    let mut k = vec![false; g.claims.len()];
    loop {
        let (u, _) = gamma(g, genesis, &k);
        let (k2, owners) = gamma(g, genesis, &u);
        if k2 == k {
            return owners;
        }
        k = k2;
    }
}

/// With the owner set fixed, retraction is a plain recursion over the retract
/// forest. Iterative post-order so long retract chains cannot overflow the stack.
fn retracted(g: &Graph, owners: &HashSet<String>) -> Result<Vec<bool>> {
    let mut memo: Vec<Option<bool>> = vec![None; g.claims.len()];
    for start in 0..g.claims.len() {
        let mut stack = vec![(start, false)];
        while let Some((x, expanded)) = stack.pop() {
            if memo[x].is_some() {
                continue;
            }
            let rs = g.retracts_on.get(&x).map(Vec::as_slice).unwrap_or_default();
            if !expanded {
                stack.push((x, true));
                stack.extend(rs.iter().map(|&r| (r, false)));
                continue;
            }
            let mut v = false;
            for &r in rs {
                // Children are resolved before their parent is popped again: each
                // retract has one target, so no node is reachable twice.
                let r_ret = memo[r].ok_or_else(|| Error::internal("retract graph is not a forest"))?;
                let (rc, xc) = (&g.claims[r], &g.claims[x]);
                v |= rules::retract_effective(&rc.o, rc.author == xc.author, owners.contains(&rc.author), r_ret);
            }
            memo[x] = Some(v);
        }
    }
    memo.into_iter().map(|m| m.ok_or_else(|| Error::internal("retract evaluation left a claim undecided"))).collect()
}

/// Computes the policy state from the claims currently in the index.
pub(crate) fn compute(conn: &Connection, genesis_owner: &str) -> Result<PolicyState> {
    let g = load_graph(conn)?;
    let owners = owners(&g, genesis_owner);
    let ret = retracted(&g, &owners)?;

    let mut roles: BTreeMap<String, BTreeSet<u8>> = BTreeMap::new();
    let mut auto: BTreeSet<String> = BTreeSet::new();
    // latest (at, id) self-declared kind per actor
    let mut kinds: BTreeMap<String, (i64, String, ActorKind)> = BTreeMap::new();
    for (i, c) in g.claims.iter().enumerate() {
        if ret[i] {
            continue;
        }
        let by_owner = owners.contains(&c.author);
        match c.p.as_str() {
            vocab::ROLE if by_owner && c.s_is_actor => {
                if let Value::Term(t) = &c.o
                    && let Some(role) = rules::role_of(t.as_str())
                {
                    roles.entry(c.s.clone()).or_default().insert(rules::role_rank(role));
                }
            }
            vocab::AUTO_ACCEPT if by_owner && c.s_is_actor && c.o == Value::Bool(true) => {
                auto.insert(c.s.clone());
            }
            vocab::ACTOR_KIND if c.s == c.author => {
                if let Value::Term(t) = &c.o
                    && let Some(kind) = rules::kind_of(t.as_str())
                {
                    let newer = kinds.get(&c.s).is_none_or(|(at, id, _)| (c.at, &c.id) > (*at, id));
                    if newer {
                        kinds.insert(c.s.clone(), (c.at, c.id.clone(), kind));
                    }
                }
            }
            _ => {}
        }
    }

    const BY_RANK: [Role; 5] = [Role::Owner, Role::Member, Role::Agent, Role::Worker, Role::Viewer];
    let member_rank = rules::role_rank(Role::Member);
    let actors: BTreeSet<&String> = owners.iter().chain(roles.keys()).chain(auto.iter()).chain(kinds.keys()).collect();
    let mut state = PolicyState::new();
    for a in actors {
        let owner = owners.contains(a);
        let held = roles.get(a);
        let member = held.is_some_and(|r| r.contains(&member_rank));
        // Ownership comes from the fixpoint above, never from a lone grant.
        let role = if owner {
            Some(Role::Owner)
        } else {
            held.and_then(|r| r.iter().find(|&&rank| rank != rules::role_rank(Role::Owner)).map(|&rank| BY_RANK[usize::from(rank)]))
        };
        state.insert(
            a.clone(),
            ActorPolicy { owner, member, auto: auto.contains(a), role, kind: kinds.get(a).map(|(_, _, k)| *k) },
        );
    }
    Ok(state)
}
