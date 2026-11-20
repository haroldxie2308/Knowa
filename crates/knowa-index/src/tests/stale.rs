//! DESIGN §4.7 stale: transitive over `src`, driven by rejected/retracted/superseded.

use knowa_model::{Id, Status, vocab};

use super::kit::*;

const A: usize = 1;

fn derived(w: &mut World, who: usize, from: &[&knowa_model::Claim], label: &str) -> knowa_model::Claim {
    let src = from.iter().map(|c| c.id.clone()).collect();
    w.with(who, &ent(), vocab::SUMMARY, text(label), |d| d.src = src)
}

#[test]
fn stale_is_transitive_and_reverts() {
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let c1 = w.label(G, &ent(), "input");
    let c2 = derived(&mut w, A, &[&c1], "insight");
    let c3 = derived(&mut w, A, &[&c2], "insight about insight");
    apply(&mut idx, &[&g, &c1, &c2, &c3]);
    assert!(!stale(&idx, &c2) && !stale(&idx, &c3));

    let r = w.retract(G, &c1);
    apply(&mut idx, &[&r]);
    assert!(!stale(&idx, &c1), "the retracted claim itself is not stale");
    assert!(stale(&idx, &c2));
    assert!(stale(&idx, &c3), "stale propagates transitively");
    assert_eq!(status(&idx, &c3), Status::Proposed, "stale is independent of status");

    let undo = w.retract(G, &r);
    apply(&mut idx, &[&undo]);
    assert!(!stale(&idx, &c2) && !stale(&idx, &c3));
}

#[test]
fn rejected_and_superseded_inputs_make_dependents_stale() {
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let e = ent();
    let a1 = w.label(A, &e, "agent input");
    let d1 = derived(&mut w, G, &[&a1], "from agent");
    let o1 = w.label(G, &e, "owner input");
    let d2 = derived(&mut w, G, &[&o1], "from owner");
    apply(&mut idx, &[&g, &a1, &d1, &o1, &d2]);

    let rej = w.review(G, &a1, false);
    let newer = w.with(G, &e, vocab::LABEL, text("owner input v2"), |d| d.replaces = vec![o1.id.clone()]);
    apply(&mut idx, &[&rej, &newer]);
    assert!(stale(&idx, &d1), "rejected input");
    assert_eq!(status(&idx, &o1), Status::Superseded);
    assert!(stale(&idx, &d2), "superseded input");

    // a proposed (not accepted) replacement does not supersede, so no staleness
    let c = w.label(G, &e, "kept");
    let d3 = derived(&mut w, G, &[&c], "from kept");
    let proposal = w.with(A, &e, vocab::LABEL, text("agent's take"), |d| d.replaces = vec![c.id.clone()]);
    apply(&mut idx, &[&c, &d3, &proposal]);
    assert!(!stale(&idx, &d3));
}

#[test]
fn inputs_arriving_late_or_unknown() {
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let c1 = w.label(G, &ent(), "input");
    let r = w.retract(G, &c1);
    let blob = Id::blob_of(b"raw");
    let c2 = w.with(A, &ent(), vocab::SUMMARY, text("x"), |d| d.src = vec![c1.id.clone(), blob]);
    apply(&mut idx, &[&g, &c2]);
    assert!(!stale(&idx, &c2), "unknown inputs and blob inputs do not make a claim stale");
    apply(&mut idx, &[&r]);
    assert!(!stale(&idx, &c2), "a retract of an absent claim changes nothing yet");
    apply(&mut idx, &[&c1]);
    assert_eq!(status(&idx, &c1), Status::Retracted);
    assert!(stale(&idx, &c2), "the input arrived already retracted");
}
