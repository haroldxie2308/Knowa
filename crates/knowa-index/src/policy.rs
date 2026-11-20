//! Policy state (DESIGN §4.6): owner set, members, autoAccept, roles and declared
//! actor kinds. A pure function of the `knowa:role` / `knowa:autoAccept` /
//! `knowa:actorKind` claims and the retract chains above them.
//!
//! - A role or autoAccept claim is *effective* iff its author is an owner and it is
//!   not retracted. Both are single-valued: the latest effective claim on an actor
//!   (by `at`, then id) wins, so a later grant demotes and `false` revokes.
//!   Retracting the latest falls back to the previous effective one.
//! - Only an actor's first self-declared actorKind claim (lowest seq) counts. If it
//!   is retracted or names no kind, the actor has no kind; a later declaration never
//!   takes over, or an agent could retract its own kind and re-declare itself Human.
//!
//! Owners need care. X is an owner iff X is the genesis owner, or X's latest
//! effective role is `knowa:Owner`, i.e. some effective Owner grant on X is not
//! *demoted* by a later effective non-Owner role claim on X. A retract counts only
//! if its author is an owner (or the target's author) and it is not retracted.
//! Ownership therefore depends on itself through two negations (retracted,
//! demoted). We take the well-founded model (Van Gelder's alternating fixpoint): in
//! every replica where the rules have a single consistent answer this is that
//! answer; where owners revoke or demote each other in a cycle, the actors whose
//! ownership is undecidable are not owners. It is a pure function of the claim set,
//! so incremental indexing and rebuild agree.

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
    seq: i64,
    at: i64,
    s: String,
    s_is_actor: bool,
    p: String,
    o: Value,
}

impl PClaim {
    /// Order of single-valued policy claims: latest `at`, then id (DESIGN §4.6).
    fn key(&self) -> (i64, &str) {
        (self.at, &self.id)
    }
}

/// A `knowa:Owner` grant to an actor, with the role claims that would demote it: later
/// (by `at`, then id) claims on the same actor naming another role.
struct OwnerGrant {
    claim: usize,
    demoters: Vec<usize>,
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
    /// role claims on an actor id naming a role; other values are not role claims
    roles: Vec<(usize, Role)>,
    owner_grants: Vec<OwnerGrant>,
}

const BASE_PREDICATES: [&str; 3] = [vocab::ROLE, vocab::AUTO_ACCEPT, vocab::ACTOR_KIND];

fn load_graph(conn: &Connection) -> Result<Graph> {
    fn read(row: &rusqlite::Row<'_>) -> Result<PClaim> {
        let s: String = row.get(4).map_err(db)?;
        let o: String = row.get(6).map_err(db)?;
        Ok(PClaim {
            id: row.get(0).map_err(db)?,
            author: row.get(1).map_err(db)?,
            seq: row.get(2).map_err(db)?,
            at: row.get(3).map_err(db)?,
            s_is_actor: id_from(&s)?.kind() == IdKind::Actor,
            s,
            p: row.get(5).map_err(db)?,
            o: value_from(&o)?,
        })
    }
    let mut g = Graph::default();
    let mut base = conn
        .prepare_cached("SELECT id, actor, seq, at, s, p, o FROM claims WHERE p IN (?1, ?2, ?3) ORDER BY id")
        .map_err(db)?;
    let mut rows = base.query(BASE_PREDICATES).map_err(db)?;
    while let Some(row) = rows.next().map_err(db)? {
        g.claims.push(read(row)?);
    }
    let mut frontier: Vec<usize> = (0..g.claims.len()).collect();
    let mut on = conn
        .prepare_cached("SELECT id, actor, seq, at, s, p, o FROM claims WHERE s = ?1 AND p = ?2 ORDER BY id")
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

    let mut by_actor: BTreeMap<&str, Vec<(usize, Role)>> = BTreeMap::new();
    for (i, c) in g.claims.iter().enumerate() {
        if c.p == vocab::ROLE
            && c.s_is_actor
            && let Value::Term(t) = &c.o
            && let Some(role) = rules::role_of(t.as_str())
        {
            g.roles.push((i, role));
            by_actor.entry(c.s.as_str()).or_default().push((i, role));
        }
    }
    let mut owner_grants = Vec::new();
    for mut claims in by_actor.into_values() {
        claims.sort_by(|a, b| g.claims[a.0].key().cmp(&g.claims[b.0].key()));
        for (k, &(i, role)) in claims.iter().enumerate() {
            if role == Role::Owner {
                let demoters = claims[k + 1..].iter().filter(|(_, r)| *r != Role::Owner).map(|(d, _)| *d).collect();
                owner_grants.push(OwnerGrant { claim: i, demoters });
            }
        }
    }
    g.owner_grants = owner_grants;
    Ok(g)
}

/// The negated atoms of the policy rules: "claim i is retracted" and "owner grant k
/// is demoted".
#[derive(Clone, PartialEq, Eq)]
struct Neg {
    retracted: Vec<bool>,
    demoted: Vec<bool>,
}

/// Γ(neg): the least model of the policy rules where every negated atom is read from
/// `neg` ("not retracted" = `!neg.retracted[x]`, "not demoted" = `!neg.demoted[k]`).
/// Returns that model's negatable atoms and owners. Anti-monotone in `neg`.
fn gamma(g: &Graph, genesis: &str, neg: &Neg) -> (Neg, HashSet<String>) {
    let mut owners = HashSet::from([genesis.to_string()]);
    let mut ret = vec![false; g.claims.len()];
    loop {
        let mut changed = false;
        for (k, og) in g.owner_grants.iter().enumerate() {
            let c = &g.claims[og.claim];
            if !neg.retracted[og.claim] && !neg.demoted[k] && owners.contains(&c.author) && owners.insert(c.s.clone()) {
                changed = true;
            }
        }
        for &(r, t) in &g.retracts {
            let (rc, tc) = (&g.claims[r], &g.claims[t]);
            if !ret[t] && rules::retract_effective(&rc.o, rc.author == tc.author, owners.contains(&rc.author), neg.retracted[r]) {
                ret[t] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Nothing above reads `demoted` of this model, so it is derived once, at the end.
    let demoted = g
        .owner_grants
        .iter()
        .map(|og| og.demoters.iter().any(|&d| !neg.retracted[d] && owners.contains(&g.claims[d].author)))
        .collect();
    (Neg { retracted: ret, demoted }, owners)
}

/// Owners in the well-founded model. K grows monotonically (Γ² is monotone), so the
/// loop ends after at most |claims| + |owner grants| + 1 rounds.
fn owners(g: &Graph, genesis: &str) -> HashSet<String> {
    let mut k = Neg { retracted: vec![false; g.claims.len()], demoted: vec![false; g.owner_grants.len()] };
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

/// Keeps, per actor, the value of the claim with the greatest `key`.
fn keep_latest<'a, T>(m: &mut BTreeMap<&'a str, ((i64, &'a str), T)>, c: &'a PClaim, v: T) {
    if m.get(c.s.as_str()).is_none_or(|(k, _)| c.key() > *k) {
        m.insert(c.s.as_str(), (c.key(), v));
    }
}

/// Computes the policy state from the claims currently in the index.
pub(crate) fn compute(conn: &Connection, genesis_owner: &str) -> Result<PolicyState> {
    let g = load_graph(conn)?;
    let owners = owners(&g, genesis_owner);
    let ret = retracted(&g, &owners)?;
    let effective = |i: usize| !ret[i] && owners.contains(&g.claims[i].author);

    let mut roles: BTreeMap<&str, ((i64, &str), Role)> = BTreeMap::new();
    for &(i, role) in &g.roles {
        if effective(i) {
            keep_latest(&mut roles, &g.claims[i], role);
        }
    }
    let mut auto: BTreeMap<&str, ((i64, &str), bool)> = BTreeMap::new();
    // first self-declared actorKind per actor: (seq, id) ascending, index
    let mut first_kind: BTreeMap<&str, ((i64, &str), usize)> = BTreeMap::new();
    for (i, c) in g.claims.iter().enumerate() {
        match c.p.as_str() {
            vocab::AUTO_ACCEPT if c.s_is_actor && effective(i) => {
                if let Value::Bool(on) = c.o {
                    keep_latest(&mut auto, c, on);
                }
            }
            vocab::ACTOR_KIND if c.s == c.author => {
                let k = (c.seq, c.id.as_str());
                if first_kind.get(c.s.as_str()).is_none_or(|(f, _)| k < *f) {
                    first_kind.insert(c.s.as_str(), (k, i));
                }
            }
            _ => {}
        }
    }
    let kinds: BTreeMap<&str, ActorKind> = first_kind
        .into_iter()
        .filter(|(_, (_, i))| !ret[*i])
        .filter_map(|(a, (_, i))| match &g.claims[i].o {
            Value::Term(t) => rules::kind_of(t.as_str()).map(|k| (a, k)),
            _ => None,
        })
        .collect();

    let actors: BTreeSet<&str> =
        owners.iter().map(String::as_str).chain(roles.keys().copied()).chain(auto.keys().copied()).chain(kinds.keys().copied()).collect();
    let mut state = PolicyState::new();
    for a in actors {
        let owner = owners.contains(a);
        let latest = roles.get(a).map(|(_, r)| *r);
        // Ownership comes from the fixpoint above, never from a lone grant. A latest
        // role of Owner without ownership (an undecidable cycle) shows no role.
        let role = if owner { Some(Role::Owner) } else { latest.filter(|r| *r != Role::Owner) };
        let p = ActorPolicy {
            owner,
            member: latest == Some(Role::Member),
            auto: auto.get(a).is_some_and(|(_, on)| *on),
            role,
            kind: kinds.get(a).copied(),
        };
        if p != ActorPolicy::default() {
            state.insert(a.to_string(), p);
        }
    }
    Ok(state)
}
