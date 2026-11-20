//! DESIGN §4.6–4.7 status rules, including every "effective only if" gate.

use knowa_model::{Id, Role, Status, Value, vocab};

use super::kit::*;

const H: usize = 1; // human, becomes member
const A: usize = 2; // agent
const W: usize = 3; // worker
const X: usize = 4; // human, no role

fn setup() -> (World, TempDir, crate::Index) {
    let mut w = World::new(5);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    (w, dir, idx)
}

#[test]
fn author_role_decides_default_status() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let by_owner = w.label(G, &e, "owner");
    let by_agent = w.label(A, &e, "agent");
    let by_stranger = w.label(X, &e, "stranger");
    apply(&mut idx, &[&by_owner, &by_agent, &by_stranger]);
    assert_eq!(status(&idx, &by_owner), Status::Accepted);
    assert_eq!(status(&idx, &by_agent), Status::Proposed);
    assert_eq!(status(&idx, &by_stranger), Status::Proposed);

    // granting membership later accepts the member's existing claims
    let grant = w.grant(G, X, vocab::ROLE_MEMBER);
    apply(&mut idx, &[&grant]);
    assert_eq!(status(&idx, &by_stranger), Status::Accepted);
}

#[test]
fn auto_accept_only_from_owner_and_only_true() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let member = w.grant(G, H, vocab::ROLE_MEMBER);
    let by_member = w.auto_accept(H, A, true); // member is not owner: ineffective
    let off = w.auto_accept(G, X, false);
    let a1 = w.label(A, &e, "a");
    let x1 = w.label(X, &e, "x");
    let w1 = w.label(W, &e, "w");
    apply(&mut idx, &[&member, &by_member, &off, &a1, &x1, &w1]);
    assert_eq!(status(&idx, &a1), Status::Proposed);
    assert_eq!(status(&idx, &x1), Status::Proposed);
    assert_eq!(status(&idx, &w1), Status::Proposed);

    let on = w.auto_accept(G, W, true);
    apply(&mut idx, &[&on]);
    assert_eq!(status(&idx, &w1), Status::Accepted);
    let info = idx.actors().unwrap().into_iter().find(|a| a.id == w.actor(W)).unwrap();
    assert!(info.auto_accept);

    // revoking autoAccept = retracting the grant
    let off = w.retract(G, &on);
    apply(&mut idx, &[&off]);
    assert_eq!(status(&idx, &w1), Status::Proposed);
}

#[test]
fn review_gates_and_latest_wins() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let member = w.grant(G, H, vocab::ROLE_MEMBER);
    let c = w.label(A, &e, "draft");
    let self_review = w.review(A, &c, true);
    let stranger_review = w.review(X, &c, true);
    apply(&mut idx, &[&member, &c, &self_review, &stranger_review]);
    assert_eq!(status(&idx, &c), Status::Proposed, "agent and non-member reviews are ineffective");

    let reject = w.review(H, &c, false);
    apply(&mut idx, &[&reject]);
    assert_eq!(status(&idx, &c), Status::Rejected, "member review is effective");

    let accept = w.review(G, &c, true);
    apply(&mut idx, &[&accept]);
    assert_eq!(status(&idx, &c), Status::Accepted, "latest review wins");

    // equal `at`: the larger id wins
    let t = w.clock;
    let r1 = w.review(G, &c, false);
    w.clock = t;
    let r2 = w.review(H, &c, true);
    apply(&mut idx, &[&r1, &r2]);
    let expected = if r1.id > r2.id { Status::Rejected } else { Status::Accepted };
    assert_eq!(status(&idx, &c), expected);
}

#[test]
fn retracted_review_is_ineffective() {
    let (mut w, _d, mut idx) = setup();
    let c = w.label(A, &ent(), "x");
    let reject = w.review(G, &c, false);
    apply(&mut idx, &[&c, &reject]);
    assert_eq!(status(&idx, &c), Status::Rejected);
    let undo = w.retract(G, &reject);
    apply(&mut idx, &[&undo]);
    assert_eq!(status(&idx, &c), Status::Proposed);
    assert_eq!(status(&idx, &reject), Status::Retracted);
}

#[test]
fn retract_gates() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let owners = w.label(G, &e, "owner's");
    let agents = w.label(A, &e, "agent's");
    let strangers = w.label(X, &e, "stranger's");
    let other = w.label(G, &e, "other");
    let by_agent_on_owner = w.retract(A, &owners);
    let by_author = w.retract(A, &agents);
    let by_owner = w.retract(G, &strangers);
    let not_true = w.say(G, &other.id, vocab::RETRACT, Value::Bool(false));
    apply(&mut idx, &[&owners, &agents, &strangers, &other, &by_agent_on_owner, &by_author, &by_owner, &not_true]);
    assert_eq!(status(&idx, &owners), Status::Accepted, "non-author non-owner retract is ineffective");
    assert_eq!(status(&idx, &agents), Status::Retracted, "author may retract");
    assert_eq!(status(&idx, &strangers), Status::Retracted, "owner may retract anyone's claim");
    assert_eq!(status(&idx, &other), Status::Accepted, "a retract must be {{bool:true}}");
}

#[test]
fn retract_of_retract_restores_the_target() {
    let (mut w, _d, mut idx) = setup();
    let c = w.label(G, &ent(), "x");
    let r1 = w.retract(G, &c);
    apply(&mut idx, &[&c, &r1]);
    assert_eq!(status(&idx, &c), Status::Retracted);

    let by_agent = w.retract(A, &r1);
    apply(&mut idx, &[&by_agent]);
    assert_eq!(status(&idx, &c), Status::Retracted, "agent cannot retract the owner's retract");

    let r2 = w.retract(G, &r1);
    apply(&mut idx, &[&r2]);
    assert_eq!(status(&idx, &r1), Status::Retracted);
    assert_eq!(status(&idx, &c), Status::Accepted);

    let r3 = w.retract(G, &r2);
    apply(&mut idx, &[&r3]);
    assert_eq!(status(&idx, &c), Status::Retracted);
}

#[test]
fn replacement_supersedes_only_when_its_base_status_is_accepted() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let member = w.grant(G, H, vocab::ROLE_MEMBER);
    let c = w.label(G, &e, "old");
    let c2 = w.with(A, &e, vocab::LABEL, text("new"), |d| d.replaces = vec![c.id.clone()]);
    apply(&mut idx, &[&member, &c, &c2]);
    assert_eq!(status(&idx, &c), Status::Accepted, "a proposed replacement does not supersede");

    let ok = w.review(H, &c2, true);
    apply(&mut idx, &[&ok]);
    assert_eq!(status(&idx, &c2), Status::Accepted);
    assert_eq!(status(&idx, &c), Status::Superseded);

    // c2 itself superseded by an accepted c3: c stays superseded, because c2's base
    // status (its review) is still accepted
    let c3 = w.with(G, &e, vocab::LABEL, text("newest"), |d| d.replaces = vec![c2.id.clone()]);
    apply(&mut idx, &[&c3]);
    assert_eq!(status(&idx, &c2), Status::Superseded);
    assert_eq!(status(&idx, &c), Status::Superseded);

    // retracting c3 un-supersedes c2 (back to its review) and keeps c superseded
    let undo = w.retract(G, &c3);
    apply(&mut idx, &[&undo]);
    assert_eq!(status(&idx, &c2), Status::Accepted);
    assert_eq!(status(&idx, &c), Status::Superseded);

    // rejecting c2 restores c
    let no = w.review(G, &c2, false);
    apply(&mut idx, &[&no]);
    assert_eq!(status(&idx, &c), Status::Accepted);
}

#[test]
fn owner_set_is_a_fixpoint_and_revocable_by_retract() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let g1 = w.grant(G, X, vocab::ROLE_OWNER);
    let g2 = w.grant(X, H, vocab::ROLE_OWNER); // granted by a granted owner
    let bogus = w.grant(A, W, vocab::ROLE_OWNER); // agent is no owner
    let h1 = w.label(H, &e, "by H");
    let w1 = w.label(W, &e, "by W");
    let a1 = w.label(A, &e, "by A");
    let h_review = w.review(H, &a1, true);
    // arrival order deliberately reversed relative to authorship
    apply(&mut idx, &[&h_review, &a1, &w1, &h1, &bogus, &g2, &g1]);
    assert_eq!(status(&idx, &h1), Status::Accepted);
    assert_eq!(status(&idx, &w1), Status::Proposed);
    assert_eq!(status(&idx, &a1), Status::Accepted, "review by a fixpoint owner is effective");
    let ids: Vec<Id> = (0..5).map(|i| w.actor(i)).collect();
    let role = |idx: &crate::Index, who: usize| idx.actors().unwrap().into_iter().find(|a| a.id == ids[who]).and_then(|a| a.role);
    assert_eq!(role(&idx, H), Some(Role::Owner));
    assert_eq!(role(&idx, W), None);

    // revoking X's ownership invalidates X's grant to H, transitively
    let revoke = w.retract(G, &g1);
    apply(&mut idx, &[&revoke]);
    assert_eq!(role(&idx, X), None);
    assert_eq!(role(&idx, H), None);
    assert_eq!(status(&idx, &h1), Status::Proposed);
    assert_eq!(status(&idx, &a1), Status::Proposed, "H's review lost its effect");

    // and retracting the revocation restores everything
    let restore = w.retract(G, &revoke);
    apply(&mut idx, &[&restore]);
    assert_eq!(role(&idx, H), Some(Role::Owner));
    assert_eq!(status(&idx, &h1), Status::Accepted);
    assert_eq!(status(&idx, &a1), Status::Accepted);
}

#[test]
fn granted_owner_can_revoke_and_retract() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let g1 = w.grant(G, X, vocab::ROLE_OWNER);
    let m = w.grant(G, H, vocab::ROLE_MEMBER);
    let h1 = w.label(H, &e, "h");
    let revoke_member = w.retract(X, &m); // X is an owner via g1
    let a1 = w.label(A, &e, "a");
    let x_retracts = w.retract(X, &a1);
    apply(&mut idx, &[&g1, &m, &h1, &revoke_member, &a1, &x_retracts]);
    assert_eq!(status(&idx, &h1), Status::Proposed);
    assert_eq!(status(&idx, &a1), Status::Retracted);
}

#[test]
fn mutual_owner_revocation_is_resolved_conservatively_and_order_independently() {
    let mut w = World::new(5);
    let g = w.genesis();
    let g1 = w.grant(G, H, vocab::ROLE_OWNER);
    let g2 = w.grant(G, X, vocab::ROLE_OWNER);
    let r1 = w.retract(H, &g2);
    let r2 = w.retract(X, &g1);
    let hx = w.label(H, &ent(), "h");
    let claims = [&g, &g1, &g2, &r1, &r2, &hx];
    let mut results = Vec::new();
    for order in [[0, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [0, 3, 1, 4, 2, 5]] {
        let dir = TempDir::new();
        let mut idx = w.index(&dir);
        for i in order {
            apply(&mut idx, &[claims[i]]);
        }
        let owners: Vec<_> = idx.actors().unwrap().into_iter().filter(|a| a.role == Some(Role::Owner)).map(|a| a.id).collect();
        let statuses: Vec<_> = claims.iter().map(|c| status(&idx, c)).collect();
        results.push((owners, statuses));
    }
    assert!(results.windows(2).all(|p| p[0] == p[1]));
    // neither mutually-revoking owner is an owner: their standing is undecidable
    assert_eq!(results[0].0, vec![w.owner()]);
}

#[test]
fn actor_kind_is_self_declared_only() {
    let (mut w, _d, mut idx) = setup();
    let own = w.kind(A, vocab::KIND_AGENT);
    let a_id = w.actor(A);
    let forged = w.say(G, &a_id, vocab::ACTOR_KIND, term(vocab::KIND_HUMAN));
    apply(&mut idx, &[&own, &forged]);
    let info = idx.actors().unwrap().into_iter().find(|a| a.id == a_id).unwrap();
    assert_eq!(info.kind, Some(knowa_model::ActorKind::Agent));
}

#[test]
fn supersession_requires_the_replacers_base_status_to_be_accepted() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let c1 = w.label(G, &e, "one");
    let c2 = w.label(G, &e, "two");
    let r1 = w.with(A, &e, vocab::LABEL, text("merged"), |d| d.replaces = vec![c1.id.clone(), c2.id.clone()]);
    let r2 = w.with(A, &e, vocab::LABEL, text("merged again"), |d| d.replaces = vec![r1.id.clone()]);
    apply(&mut idx, &[&c1, &c2, &r1, &r2]);
    let ok = w.review(G, &r2, true);
    apply(&mut idx, &[&ok]);
    assert_eq!(status(&idx, &r2), Status::Accepted);
    assert_eq!(status(&idx, &r1), Status::Superseded);
    assert_eq!(status(&idx, &c1), Status::Accepted, "r1 is merely superseded; its base status is proposed");
    assert_eq!(status(&idx, &c2), Status::Accepted);

    // accepting r1 gives it an accepted base status, so it supersedes while superseded
    let ok1 = w.review(G, &r1, true);
    apply(&mut idx, &[&ok1]);
    assert_eq!(status(&idx, &r1), Status::Superseded);
    assert_eq!(status(&idx, &c1), Status::Superseded);
    assert_eq!(status(&idx, &c2), Status::Superseded);

    // a retracted replacement supersedes nothing, whatever its review says
    let undo = w.retract(G, &r1);
    apply(&mut idx, &[&undo]);
    assert_eq!(status(&idx, &c1), Status::Accepted);
}

#[test]
fn only_the_first_actor_kind_counts() {
    let (mut w, _d, mut idx) = setup();
    let first = w.kind(A, vocab::KIND_AGENT);
    let relabel = w.kind(A, vocab::KIND_HUMAN);
    let e = ent();
    let said = w.label(A, &e, "agent says");
    // the later declaration arrives first
    apply(&mut idx, &[&relabel, &said, &first]);
    let a_id = w.actor(A);
    let kind = |idx: &crate::Index| idx.actors().unwrap().into_iter().find(|a| a.id == a_id).unwrap().kind;
    assert_eq!(kind(&idx), Some(knowa_model::ActorKind::Agent));
    let humans = knowa_model::query::View { actor_kinds: vec![knowa_model::ActorKind::Human], ..Default::default() };
    let about = knowa_model::query::Where { s: vec![e.clone()], ..Default::default() };
    let got = idx.find_claims(&about, &humans, knowa_model::query::Order::Recorded, 10, None).unwrap().items;
    assert!(got.is_empty(), "a re-declared Human kind does not make the agent's claims human");

    // retracting the first declaration leaves no kind; a later one never takes over
    let undo = w.retract(A, &first);
    apply(&mut idx, &[&undo]);
    assert_eq!(kind(&idx), None);
}

#[test]
fn role_and_auto_accept_are_latest_wins() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let ids: Vec<Id> = (0..5).map(|i| w.actor(i)).collect();
    let info = |idx: &crate::Index, who: usize| idx.actors().unwrap().into_iter().find(|a| a.id == ids[who]);
    let role = |idx: &crate::Index, who: usize| info(idx, who).and_then(|a| a.role);

    let member = w.grant(G, X, vocab::ROLE_MEMBER);
    let viewer = w.grant(G, X, vocab::ROLE_VIEWER);
    let by_x = w.label(X, &e, "x");
    apply(&mut idx, &[&viewer, &by_x, &member]);
    assert_eq!(role(&idx, X), Some(Role::Viewer), "a later grant demotes");
    assert_eq!(status(&idx, &by_x), Status::Proposed);

    let undo = w.retract(G, &viewer);
    apply(&mut idx, &[&undo]);
    assert_eq!(role(&idx, X), Some(Role::Member), "retracting the latest falls back to the previous one");
    assert_eq!(status(&idx, &by_x), Status::Accepted);

    let by_agent = w.grant(A, X, vocab::ROLE_VIEWER);
    apply(&mut idx, &[&by_agent]);
    assert_eq!(role(&idx, X), Some(Role::Member), "a grant by a non-owner is not effective, so it is not the latest");

    let on = w.auto_accept(G, W, true);
    let off = w.auto_accept(G, W, false);
    let by_w = w.label(W, &e, "w");
    apply(&mut idx, &[&off, &by_w, &on]);
    assert_eq!(status(&idx, &by_w), Status::Proposed, "autoAccept false revokes");
    assert!(!info(&idx, W).unwrap().auto_accept);

    // demoting an owner removes ownership and everything that depended on it
    let owner = w.grant(G, H, vocab::ROLE_OWNER);
    let by_h = w.label(H, &e, "h");
    let h_grants = w.grant(H, A, vocab::ROLE_MEMBER);
    let by_a = w.label(A, &e, "a");
    apply(&mut idx, &[&owner, &by_h, &h_grants, &by_a]);
    assert_eq!(role(&idx, H), Some(Role::Owner));
    assert_eq!(status(&idx, &by_a), Status::Accepted);
    let demote = w.grant(G, H, vocab::ROLE_MEMBER);
    apply(&mut idx, &[&demote]);
    assert_eq!(role(&idx, H), Some(Role::Member));
    assert_eq!(status(&idx, &by_h), Status::Accepted, "still a member");
    assert_eq!(role(&idx, A), None, "H's grant needs H to be an owner");
    assert_eq!(status(&idx, &by_a), Status::Proposed);
}
