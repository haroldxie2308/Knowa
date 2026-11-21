//! View filtering, entity matching, display values, paging, feed, equivalence keys,
//! edges and export selection.

use std::collections::BTreeMap;

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorKind, Dec, Dir, ErrorCode, Id, Status, Term, Value, Visibility, vocab};

use super::kit::*;

const A: usize = 1; // agent
const X: usize = 2; // no declared kind
const H: usize = 3; // human member

fn setup() -> (World, TempDir, crate::Index) {
    let mut w = World::new(4);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let k0 = w.kind(G, vocab::KIND_HUMAN);
    let ka = w.kind(A, vocab::KIND_AGENT);
    let kh = w.kind(H, vocab::KIND_HUMAN);
    let m = w.grant(G, H, vocab::ROLE_MEMBER);
    apply(&mut idx, &[&g, &k0, &ka, &kh, &m]);
    (w, dir, idx)
}

fn claim_ids(idx: &crate::Index, w: &Where, v: &View) -> Vec<Id> {
    let mut out: Vec<Id> = idx.find_claims(w, v, Order::Recorded, 1000, None).unwrap().items.into_iter().map(|c| c.claim.id).collect();
    out.sort();
    out
}

fn sorted(mut v: Vec<Id>) -> Vec<Id> {
    v.sort();
    v
}

#[test]
fn view_filters_actor_kinds_stale_and_confidence() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let by_g = w.label(G, &e, "g");
    let by_a = w.label(A, &e, "a");
    let by_x = w.label(X, &e, "x");
    let low = w.with(G, &e, vocab::BODY, text("low"), |d| d.conf = Some(Dec::parse("0.4").unwrap()));
    let half = w.with(G, &e, vocab::BODY, text("half"), |d| d.conf = Some(Dec::parse("0.50").unwrap()));
    let derived = w.with(G, &e, vocab::SUMMARY, text("derived"), |d| d.src = vec![by_a.id.clone()]);
    let rej = w.review(G, &by_a, false);
    apply(&mut idx, &[&by_g, &by_a, &by_x, &low, &half, &derived, &rej]);
    let about = Where { s: vec![e.clone()], ..Where::default() };

    let humans = View { actor_kinds: vec![ActorKind::Human], ..all_statuses() };
    assert_eq!(claim_ids(&idx, &about, &humans), sorted(vec![by_g.id.clone(), low.id.clone(), half.id.clone(), derived.id.clone()]));
    let agents = View { actor_kinds: vec![ActorKind::Agent], ..all_statuses() };
    assert_eq!(claim_ids(&idx, &about, &agents), vec![by_a.id.clone()]);
    let any_kind = View { actor_kinds: vec![ActorKind::Human, ActorKind::Agent, ActorKind::Worker], ..all_statuses() };
    assert!(!claim_ids(&idx, &about, &any_kind).contains(&by_x.id), "an actor without a declared kind matches no kind");

    assert!(claim_ids(&idx, &about, &View::default()).contains(&derived.id), "stale included by default");
    let fresh = View { include_stale: false, ..View::default() };
    assert!(!claim_ids(&idx, &about, &fresh).contains(&derived.id));
    assert!(!claim_ids(&idx, &about, &View::default()).contains(&by_a.id), "rejected is not in the default view");

    let confident = View { min_conf: Some("0.5".into()), ..View::default() };
    let got = claim_ids(&idx, &about, &confident);
    assert!(got.contains(&half.id) && got.contains(&by_g.id), "0.50 ≥ 0.5 exactly; missing conf counts as 1");
    assert!(!got.contains(&low.id));
    for bad in ["1.5", "-0.1", "abc", ".5"] {
        let v = View { min_conf: Some(bad.into()), ..View::default() };
        assert_eq!(idx.find_claims(&about, &v, Order::Recorded, 10, None).err().unwrap().code, ErrorCode::Invalid, "{bad}");
    }
}

#[test]
fn entity_matches_when_one_claim_passes_view_and_claim_filters_together() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let label = w.label(G, &e, "accepted label");
    let typ = w.say(A, &e, vocab::TYPE, term(vocab::T_PERSON));
    apply(&mut idx, &[&label, &typ]);
    let proposed = View { status: vec![Status::Proposed], ..View::default() };
    let p = |t: &str| Where { p: vec![Term::parse(t).unwrap()], ..Where::default() };
    let find = |w: &Where, v: &View| idx.find_entities(w, v, Order::Recorded, 10, None).unwrap().items.into_iter().map(|e| e.id).collect::<Vec<_>>();
    assert!(find(&p(vocab::LABEL), &proposed).is_empty(), "the label is not proposed and the proposed claim is not a label");
    assert_eq!(find(&p(vocab::TYPE), &proposed), vec![e.clone()]);
    assert_eq!(find(&p(vocab::LABEL), &View::default()), vec![e.clone()]);
    let by_agent = Where { actors: vec![w.actor(A)], p: vec![Term::parse(vocab::LABEL).unwrap()], ..Where::default() };
    assert!(find(&by_agent, &View::default()).is_empty());
    let typed = Where { type_: vec![Term::parse(vocab::T_PERSON).unwrap()], ..Where::default() };
    assert_eq!(find(&typed, &View::default()), vec![e.clone()]);
    let o = Where { o: Some(text("accepted label")), ..Where::default() };
    assert_eq!(find(&o, &View::default()), vec![e.clone()]);

    let rejected_only = View { status: vec![Status::Rejected], ..View::default() };
    assert!(idx.entity(&e, &rejected_only).unwrap().is_none());
    let view = idx.entity(&e, &View::default()).unwrap().unwrap();
    assert_eq!(view.spaces, vec![w.space.clone()]);
    assert_eq!(view.types, vec![Term::parse(vocab::T_PERSON).unwrap()], "the proposed type passes the default view");
    let accepted = View { status: vec![Status::Accepted], ..View::default() };
    assert!(idx.entity(&e, &accepted).unwrap().unwrap().types.is_empty(), "display fields follow the view");
}

#[test]
fn display_value_prefers_accepted_then_latest() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let accepted = w.label(G, &e, "accepted, older");
    let proposed = w.label(A, &e, "proposed, newer");
    apply(&mut idx, &[&accepted, &proposed]);
    assert_eq!(idx.entity(&e, &View::default()).unwrap().unwrap().label.as_deref(), Some("accepted, older"));
    let r = w.retract(G, &accepted);
    apply(&mut idx, &[&r]);
    let view = idx.entity(&e, &View::default()).unwrap().unwrap();
    assert_eq!(view.label.as_deref(), Some("proposed, newer"));
    assert_eq!(view.claims, vec![proposed.id.clone()]);
}

#[test]
fn the_label_falls_back_to_the_display_chosen_name() {
    let (mut w, _d, mut idx) = setup();
    let file = MemBlobs::default().put(b"holiday photo bytes");
    let accepted = w.say(G, &file, vocab::NAME, text("IMG_0001.jpg"));
    let proposed = w.say(A, &file, vocab::NAME, text("IMG_0001 (copy).jpg"));
    apply(&mut idx, &[&accepted, &proposed]);

    let e = idx.entity(&file, &View::default()).unwrap().unwrap();
    assert_eq!(e.label.as_deref(), Some("IMG_0001.jpg"), "accepted before proposed, as for a label");
    assert!(e.claims.contains(&accepted.id) && !e.claims.contains(&proposed.id), "the chosen name supports the label");
    let agents = View { actor_kinds: vec![ActorKind::Agent], ..View::default() };
    assert_eq!(idx.entity(&file, &agents).unwrap().unwrap().label.as_deref(), Some("IMG_0001 (copy).jpg"), "only passing names count");
    super::evidence::assert_routes_agree(&idx, &[Where::default()], "name as label");
}

#[test]
fn a_label_beats_a_name_whatever_their_statuses() {
    let (mut w, _d, mut idx) = setup();
    let file = MemBlobs::default().put(b"scanned pages");
    let name = w.say(G, &file, vocab::NAME, text("scan_0042.pdf"));
    let label = w.label(A, &file, "Tax return 2019");
    apply(&mut idx, &[&name, &label]);
    assert_eq!(status(&idx, &label), Status::Proposed);

    let e = idx.entity(&file, &View::default()).unwrap().unwrap();
    assert_eq!(e.label.as_deref(), Some("Tax return 2019"), "a proposed label beats an accepted name");
    assert!(e.claims.contains(&label.id) && !e.claims.contains(&name.id), "an unused name supports nothing");
    let r = w.retract(A, &label);
    apply(&mut idx, &[&r]);
    assert_eq!(idx.entity(&file, &View::default()).unwrap().unwrap().label.as_deref(), Some("scan_0042.pdf"));
}

#[test]
fn fragment_subjects_are_distinct_entities() {
    let (mut w, _d, mut idx) = setup();
    let blob = Id::blob_of(b"photo");
    let face = Id::parse(&format!("{}#xywh=10,10,50,50", blob.as_str())).unwrap();
    let c1 = w.label(G, &blob, "the photo");
    let c2 = w.label(G, &face, "a face");
    let tag = w.say(G, &ent(), vocab::MENTIONS, Value::Ref(face.clone()));
    apply(&mut idx, &[&c1, &c2, &tag]);
    assert_eq!(idx.entity(&blob, &View::default()).unwrap().unwrap().label.as_deref(), Some("the photo"));
    assert_eq!(idx.entity(&face, &View::default()).unwrap().unwrap().label.as_deref(), Some("a face"));
    let (out, inbound) = idx.claims_about(&blob, &View::default(), 10).unwrap();
    assert_eq!(out.iter().map(|c| &c.claim.id).collect::<Vec<_>>(), vec![&c1.id], "s matches exactly");
    assert_eq!(inbound.iter().map(|c| &c.claim.id).collect::<Vec<_>>(), vec![&tag.id], "o.ref matches by base id");
    let edges = idx.edges(&blob, &View::default()).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!((edges[0].dir, &edges[0].via, &edges[0].to), (Dir::In, &tag.id, &tag.body.s));
    let edges = idx.edges(&tag.body.s, &View::default()).unwrap();
    assert_eq!((edges[0].dir, &edges[0].to), (Dir::Out, &face), "out edges keep the fragment");
}

#[test]
fn paging_covers_every_item_once_and_rejects_bad_cursors() {
    let (mut w, _d, mut idx) = setup();
    let mut want = Vec::new();
    for i in 0..7 {
        let e = ent();
        let c = w.label(G, &e, &format!("item {i}"));
        apply(&mut idx, &[&c]);
        want.push(e);
    }
    let mut got = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = idx.find_entities(&Where::default(), &View::default(), Order::Recorded, 3, cursor.as_deref()).unwrap();
        assert!(page.items.len() <= 3);
        got.extend(page.items.into_iter().map(|e| e.id));
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(got, want);
    for bad in ["", "garbage", "off:", "off:-1", "off:1x", "off:+1"] {
        let err = idx.find_entities(&Where::default(), &View::default(), Order::Recorded, 3, Some(bad)).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid, "{bad:?}");
        assert_eq!(idx.log(&[], None, 3, Some(bad)).err().unwrap().code, ErrorCode::Invalid);
    }
    for bad_limit in [0, knowa_model::query::MAX_LIMIT + 1] {
        assert_eq!(idx.find_claims(&Where::default(), &View::default(), Order::Recorded, bad_limit, None).err().unwrap().code, ErrorCode::Invalid);
    }
}

fn delivered(entries: &[(Id, u64)]) -> BTreeMap<Id, u64> {
    entries.iter().cloned().collect()
}

fn feed_ids(idx: &crate::Index, done: &BTreeMap<Id, u64>, limit: u32) -> Vec<Id> {
    idx.feed(done, limit).unwrap().into_iter().map(|c| c.claim.id).collect()
}

/// DESIGN §6 feed: what the per-actor map does not cover, in arrival order.
#[test]
fn feed_returns_what_no_entry_covers_in_arrival_order() {
    let mut w = World::new(3);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let (g_, a_, b_) = (w.actor(G), w.actor(1), w.actor(2));
    let g = w.genesis();
    let e = ent();
    // recorded before a1, but arrives after it
    let g2 = w.label(G, &e, "g2");
    let a1 = w.label(1, &e, "a1");
    let g3 = w.label(G, &e, "g3");
    let b1 = w.label(2, &e, "b1");
    let a2 = w.label(1, &e, "a2");
    apply(&mut idx, &[&g]);
    apply(&mut idx, &[&a1, &g2]);
    apply(&mut idx, &[&g3, &b1, &g2, &a2]); // g2 again: already indexed, no new arrival
    let arrival = vec![g.id.clone(), a1.id.clone(), g2.id.clone(), g3.id.clone(), b1.id.clone(), a2.id.clone()];

    assert_eq!(feed_ids(&idx, &delivered(&[]), 10), arrival, "arrival order, not record time");
    assert_eq!(feed_ids(&idx, &delivered(&[]), 2), arrival[..2]);
    assert_eq!(feed_ids(&idx, &delivered(&[(g_.clone(), 3)]), 10), vec![a1.id.clone(), b1.id.clone(), a2.id.clone()]);
    assert_eq!(
        feed_ids(&idx, &delivered(&[(g_.clone(), 1), (a_.clone(), 1)]), 10),
        vec![g2.id.clone(), g3.id.clone(), b1.id.clone(), a2.id.clone()],
        "an actor without an entry has had nothing delivered"
    );
    assert_eq!(feed_ids(&idx, &delivered(&[(g_.clone(), 1), (a_.clone(), 1)]), 3), vec![g2.id.clone(), g3.id.clone(), b1.id.clone()]);
    // seed [5; 32]: an actor with no claim in this index
    let elsewhere = World::new(5).actor(4);
    let past_every_head = delivered(&[(g_, 99), (a_, 2), (b_, 1), (elsewhere, 5)]);
    assert!(feed_ids(&idx, &past_every_head, 10).is_empty(), "entries past the heads, or for actors not here, cover nothing more");
    for bad in [0, knowa_model::query::MAX_LIMIT + 1] {
        assert_eq!(idx.feed(&delivered(&[]), bad).unwrap_err().code, ErrorCode::Invalid);
    }
}

/// Paging by the highest seq delivered per actor, with actors interleaved, delivers
/// every claim once, in arrival order, whatever the page size.
#[test]
fn paging_by_highest_delivered_seq_delivers_each_claim_once() {
    let mut w = World::new(3);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    let e = ent();
    let mut arrival = vec![g.id.clone()];
    for i in 0..7 {
        let batch: Vec<_> = [1, G, 2, 1].iter().map(|&who| w.label(who, &e, &format!("{who}-{i}"))).collect();
        apply(&mut idx, &batch.iter().collect::<Vec<_>>());
        arrival.extend(batch.iter().map(|c| c.id.clone()));
    }
    for limit in [1, 2, 3, 5] {
        let mut done = BTreeMap::new();
        let mut got = Vec::new();
        loop {
            let page = idx.feed(&done, limit).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= limit as usize);
            for c in page {
                let seq = done.entry(c.claim.body.actor.clone()).or_insert(0);
                *seq = (*seq).max(c.claim.body.seq);
                got.push(c.claim.id);
            }
            assert!(got.len() <= arrival.len(), "limit {limit}: more deliveries than claims");
        }
        assert_eq!(got, arrival, "limit {limit}");
    }
}

#[test]
fn reopening_keeps_the_build_and_another_space_or_an_unfinished_build_is_rebuilt() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let path = dir.path().join("index.sqlite");
    let generation = "01jd5k8v0m3q7x9yz2c4b6n8pr";
    let mut idx = crate::Index::create(&path, dir.path(), &w.space, &w.owner(), Some(generation)).unwrap();
    idx.mark_complete().unwrap();
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    let epoch = idx.epoch().to_string();
    assert_eq!(epoch.len(), 32);
    drop(idx);

    let other_space = Id::new_space().unwrap();
    assert!(
        crate::Index::open_existing(&path, dir.path(), &other_space, &w.owner(), Some(generation)).unwrap().is_none(),
        "an index of another space must be rebuilt"
    );
    assert!(
        crate::Index::open_existing(&path, dir.path(), &w.space, &w.owner(), None).unwrap().is_none(),
        "an index of another generation must be rebuilt"
    );
    let mut idx = crate::Index::open_existing(&path, dir.path(), &w.space, &w.owner(), Some(generation)).unwrap().unwrap();
    assert_eq!(idx.epoch(), epoch, "reopening at the recorded generation keeps the build");
    let e = ent();
    let c2 = w.label(G, &e, "two");
    apply(&mut idx, &[&c2]);
    assert_eq!(feed_ids(&idx, &delivered(&[]), 10), vec![g.id.clone(), c2.id.clone()], "arrivals continue after the reopen");
    drop(idx);

    let fresh = w.index(&dir);
    assert_ne!(fresh.epoch(), epoch, "a rebuilt index gets a new epoch");
    assert!(feed_ids(&fresh, &delivered(&[]), 10).is_empty());
    drop(fresh);
    assert!(crate::Index::open_existing(&path, dir.path(), &w.space, &w.owner(), None).unwrap().is_none(), "an unfinished build must be rebuilt");
}

/// The feed's per-actor reads must seek `claims_actor`, never scan every claim.
#[test]
fn the_feed_seeks_the_actor_index() {
    let (_w, _dir, idx) = setup();
    let plan = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Vec<String> {
        idx.conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params, |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let next = plan(crate::read::FEED_NEXT_SQL, &[&"a", &0]);
    assert_eq!(next, vec!["SEARCH claims USING COVERING INDEX claims_actor (actor=? AND seq>?)".to_string()], "{next:?}");
    let actors = plan(crate::read::FEED_ACTORS_SQL, &[]);
    let on_claims: Vec<&String> = actors.iter().filter(|l| l.contains("claims")).collect();
    assert!(!on_claims.is_empty(), "{actors:?}");
    assert!(on_claims.iter().all(|l| l.starts_with("SEARCH claims USING COVERING INDEX claims_actor")), "{actors:?}");
}

#[test]
fn equivalent_ignores_retracted_and_other_actors() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let c = w.label(H, &e, "same");
    let other = w.label(A, &e, "same");
    apply(&mut idx, &[&c, &other]);
    let key = c.body.equiv_key().unwrap();
    assert_eq!(key, other.body.equiv_key().unwrap());
    assert_eq!(idx.equivalent(&w.actor(H), &key).unwrap().unwrap().claim.id, c.id);
    let r = w.retract(H, &c);
    apply(&mut idx, &[&r]);
    assert!(idx.equivalent(&w.actor(H), &key).unwrap().is_none());
    assert_eq!(idx.equivalent(&w.actor(A), &key).unwrap().unwrap().claim.id, other.id);
}

#[test]
fn equivalent_only_matches_active_fresh_claims() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    // a revert: Foo, then Bar replacing Foo, then Foo again
    let foo = w.label(H, &e, "Foo");
    let bar = w.with(H, &e, vocab::LABEL, text("Bar"), |d| d.replaces = vec![foo.id.clone()]);
    apply(&mut idx, &[&foo, &bar]);
    assert_eq!(crate::tests::kit::status(&idx, &foo), Status::Superseded);
    let key = |c: &knowa_model::Claim| c.body.equiv_key().unwrap();
    assert!(idx.equivalent(&w.actor(H), &key(&foo)).unwrap().is_none(), "a superseded equivalent does not block the revert");
    assert_eq!(idx.equivalent(&w.actor(H), &key(&bar)).unwrap().unwrap().claim.id, bar.id);

    let rejected = w.label(A, &e, "nope");
    let no = w.review(G, &rejected, false);
    let input = w.label(G, &e, "input");
    let derived = w.with(H, &e, vocab::SUMMARY, text("derived"), |d| d.src = vec![input.id.clone()]);
    let gone = w.retract(G, &input);
    apply(&mut idx, &[&rejected, &no, &input, &derived, &gone]);
    assert!(idx.equivalent(&w.actor(A), &key(&rejected)).unwrap().is_none(), "rejected");
    assert!(crate::tests::kit::stale(&idx, &derived));
    assert!(idx.equivalent(&w.actor(H), &key(&derived)).unwrap().is_none(), "stale: re-deriving from new inputs creates a new claim");
}

#[test]
fn export_never_ships_private_blobs_notes_or_visibility_settings() {
    let (mut w, _d, mut idx) = setup();
    let mut blobs = MemBlobs::default();
    let private_blob = blobs.put(b"private bytes");
    let public_blob = blobs.put(b"public bytes");
    let unreferenced = blobs.put(b"public, but nothing refers to it");
    let e = ent();
    let space = w.space.clone();
    let default_public = w.say(G, &space, vocab::DEFAULT_VISIBILITY, term(vocab::VIS_PUBLIC));
    let hide_blob = w.say(G, &private_blob, vocab::VISIBILITY, term(vocab::VIS_PRIVATE));
    // public claims that refer to the private blob in every position
    let mentions = w.say(G, &e, vocab::MENTIONS, Value::Ref(private_blob.clone()));
    let derived = w.with(G, &e, vocab::SUMMARY, text("derived"), |d| d.src = vec![private_blob.clone(), public_blob.clone()]);
    let review = w.with(A, &mentions.id, vocab::REVIEW, term(vocab::REVIEW_ACCEPT), |d| d.src = vec![private_blob.clone()]);
    // a private claim and notes about it
    let secret = w.label(G, &e, "secret");
    let hide_secret = w.say(G, &secret.id, vocab::VISIBILITY, term(vocab::VIS_PRIVATE));
    let retract_secret = w.with(G, &secret.id, vocab::RETRACT, Value::Bool(true), |d| d.note = Some("the secret was X".into()));
    let review_secret = w.review(H, &secret, true);
    let unretract = w.retract(G, &retract_secret);
    // a review chain on an exported claim: exported with its target, transitively
    let unreview = w.retract(A, &review);
    let retract_derived = w.retract(G, &derived);
    apply_blobs(
        &mut idx,
        &blobs,
        &[
            &default_public, &hide_blob, &mentions, &derived, &review, &secret, &hide_secret, &retract_secret, &review_secret,
            &unretract, &unreview, &retract_derived,
        ],
    );

    for level in [Visibility::Shared, Visibility::Public] {
        let (claims, b) = idx.export_set_with(level, &blobs).unwrap();
        let has = |c: &knowa_model::Claim| claims.contains(&c.id);
        assert!(!b.contains(&private_blob), "{level:?}: referenced by exported claims, but private itself");
        assert!(has(&mentions) && has(&derived), "{level:?}: public by the space default");
        assert!(has(&review) && has(&unreview) && has(&retract_derived), "{level:?}: review/retract travel with their exported target");
        assert!(has(&default_public), "{level:?}: trust claims are always exported");
        assert!(!has(&secret), "{level:?}: private claim");
        assert!(!has(&retract_secret) && !has(&review_secret) && !has(&unretract), "{level:?}: notes about a private claim stay private");
        assert!(!has(&hide_blob) && !has(&hide_secret), "{level:?}: visibility claims are never exported");
        assert_eq!(b, sorted(vec![public_blob.clone(), unreferenced.clone()]), "{level:?}: blobs by their own visibility only");
    }
    let (all, all_blobs) = idx.export_set_with(Visibility::Private, &blobs).unwrap();
    assert!(all.contains(&secret.id) && all.contains(&hide_blob.id) && all.contains(&retract_secret.id));
    assert_eq!(
        all_blobs,
        sorted(vec![private_blob.clone(), public_blob.clone(), unreferenced.clone()]),
        "private: everything, including blobs no claim references (DESIGN §7.3)"
    );
}

#[test]
fn export_set_follows_effective_visibility() {
    let (mut w, _d, mut idx) = setup();
    let mut blobs = MemBlobs::default();
    let present = blobs.put(b"present bytes");
    let region_of = blobs.put(b"another photo");
    let absent = Id::blob_of(b"never stored");
    let (e1, e2) = (ent(), ent());
    let private = w.label(G, &e1, "private by default");
    let shared_subject = w.label(G, &e2, "shared via subject");
    let claim_private = w.label(G, &e2, "claim-level private");
    let claim_public = w.with(G, &e1, vocab::MENTIONS, Value::Ref(Id::parse(&format!("{}#xywh=0,0,1,1", region_of.as_str())).unwrap()), |d| {
        d.src = vec![absent.clone()]
    });
    let about_blob = w.label(G, &present, "a blob");
    let vis_e2 = w.say(G, &e2, vocab::VISIBILITY, term(vocab::VIS_SHARED));
    let vis_cp = w.say(G, &claim_private.id, vocab::VISIBILITY, term(vocab::VIS_PRIVATE));
    let vis_pub = w.say(G, &claim_public.id, vocab::VISIBILITY, term(vocab::VIS_PUBLIC));
    let vis_blob = w.say(G, &present, vocab::VISIBILITY, term(vocab::VIS_PUBLIC));
    let by_member = w.say(H, &e1, vocab::VISIBILITY, term(vocab::VIS_PUBLIC)); // not an owner: ineffective
    let undone = w.say(G, &private.id, vocab::VISIBILITY, term(vocab::VIS_PUBLIC));
    let undo = w.retract(G, &undone);
    apply_blobs(
        &mut idx,
        &blobs,
        &[&private, &shared_subject, &claim_private, &claim_public, &about_blob, &vis_e2, &vis_cp, &vis_pub, &vis_blob, &by_member, &undone, &undo],
    );
    let policy = |idx: &crate::Index, preds: &[&str]| -> Vec<Id> {
        idx.find_claims(&Where::default(), &all_statuses(), Order::Recorded, 1000, None)
            .unwrap()
            .items
            .into_iter()
            .filter(|c| preds.contains(&c.claim.body.p.as_str()))
            .map(|c| c.claim.id)
            .collect()
    };
    let trust_preds = [vocab::OWNER, vocab::ROLE, vocab::AUTO_ACCEPT, vocab::ACTOR_KIND, vocab::DEFAULT_VISIBILITY];
    // (non-policy claims, blobs) at a shared/public level; asserts trust claims are
    // exported and visibility claims (and retracts of them) are not
    let export = |idx: &crate::Index, level| {
        let (claims, b) = idx.export_set_with(level, &blobs).unwrap();
        let trust = policy(idx, &trust_preds);
        assert!(trust.iter().all(|p| claims.contains(p)), "{level:?}: trust claims are always exported");
        assert!(policy(idx, &[vocab::VISIBILITY]).iter().all(|p| !claims.contains(p)), "{level:?}: visibility claims never are");
        assert!(!claims.contains(&undo.id), "{level:?}: a retract travels only with an exported target");
        let content: Vec<Id> = claims.into_iter().filter(|c| !trust.contains(c)).collect();
        (sorted(content), b)
    };

    let (shared, shared_blobs) = export(&idx, Visibility::Shared);
    assert_eq!(shared, sorted(vec![shared_subject.id.clone(), claim_public.id.clone(), about_blob.id.clone()]));
    assert_eq!(shared_blobs, vec![present.clone()], "by the blob's own visibility; region_of is private by default");
    let (public, _) = export(&idx, Visibility::Public);
    assert_eq!(public, sorted(vec![claim_public.id.clone(), about_blob.id.clone()]));
    let (everything, everything_blobs) = idx.export_set_with(Visibility::Private, &blobs).unwrap();
    assert_eq!(everything.len(), 5 + policy(&idx, vocab::POLICY_PREDICATES).len(), "private exports every claim");
    assert_eq!(everything_blobs, sorted(vec![present.clone(), region_of.clone()]), "base ids, present blobs only");

    let space = w.space.clone();
    let default_shared = w.say(G, &space, vocab::DEFAULT_VISIBILITY, term(vocab::VIS_SHARED));
    apply_blobs(&mut idx, &blobs, &[&default_shared]);
    let (shared, shared_blobs) = export(&idx, Visibility::Shared);
    assert!(shared.contains(&private.id), "space default applies when nothing more specific is set");
    assert!(!shared.contains(&claim_private.id), "claim-level setting wins over the default");
    assert_eq!(shared_blobs, sorted(vec![present.clone(), region_of.clone()]), "the space default applies to blobs too");
}

#[test]
fn actors_lists_authors_and_role_holders() {
    let (mut w, _d, mut idx) = setup();
    let viewer = w.grant(G, X, vocab::ROLE_VIEWER);
    apply(&mut idx, &[&viewer]);
    let actors = idx.actors().unwrap();
    let find = |who: usize| actors.iter().find(|a| a.id == w.actor(who)).unwrap();
    assert_eq!(find(G).role, Some(knowa_model::Role::Owner));
    assert_eq!(find(G).kind, Some(ActorKind::Human));
    assert_eq!(find(H).role, Some(knowa_model::Role::Member));
    assert_eq!(find(X).role, Some(knowa_model::Role::Viewer), "listed for the role alone");
    assert_eq!(find(X).claims, 0);
    assert_eq!(find(A).kind, Some(ActorKind::Agent));
    assert_eq!(find(A).role, None);
}

/// Regression: without stats, SQLite answered the duplicate check (DESIGN §4.4) from
/// `claims_actor` because it also satisfies `ORDER BY seq`, scanning every claim of
/// the writing actor for each draft, which made ingest quadratic. The check must seek
/// on the equivalence index.
#[test]
fn the_duplicate_check_seeks_the_equivalence_index() {
    let (_w, _dir, idx) = setup();
    let plan: Vec<String> = idx
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {}", crate::read::EQUIVALENT_SQL))
        .unwrap()
        .query_map(["k", "a", "accepted", "proposed"], |r| r.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(plan, vec!["SEARCH claims USING INDEX claims_equiv (equiv=? AND actor=?)".to_string()], "{plan:?}");
}
