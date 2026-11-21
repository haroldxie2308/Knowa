//! DESIGN §4.7 "views apply to evidence": whether an entity matches `text`, `type`,
//! `when` or `near`, and its display fields, come only from claims passing the view.
//! Also: the stored default-view route returns exactly what the computed route does.

use knowa_model::query::{Near, Order, View, Where, WhenRange};
use knowa_model::view::WhenSource;
use knowa_model::{ActorKind, ClaimView, EntityView, Id, Status, Term, Value, vocab};

use super::kit::*;

/// Passes exactly the default view's claims (every conf is ≥ 0) but is not
/// syntactically the default view, so reads take the computed route.
fn computed_default() -> View {
    View { min_conf: Some("0".into()), ..View::default() }
}

fn all_pages<T>(mut next: impl FnMut(Option<&str>) -> crate::Page<T>) -> Vec<T> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = next(cursor.as_deref());
        out.extend(page.items);
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => return out,
        }
    }
}

/// Asserts the stored route (default view) and the computed route agree on every
/// query in `wheres` and every order, page by page, for entities, claims and
/// single-entity lookups. Returns how many entity results were compared, so callers
/// can check the queries were not vacuous.
pub(super) fn assert_routes_agree(idx: &crate::Index, wheres: &[Where], ctx: &str) -> usize {
    let orders = [Order::Relevance, Order::When, Order::WhenDesc, Order::Recorded, Order::RecordedDesc, Order::Distance];
    let mut compared = 0;
    for w in wheres {
        for order in orders {
            if order == Order::Distance && w.near.is_none() {
                continue;
            }
            let entities = |v: &View| all_pages(|c| idx.find_entities(w, v, order, 9, c).unwrap());
            let stored: Vec<EntityView> = entities(&View::default());
            assert_eq!(stored, entities(&computed_default()), "{ctx}: entities differ for {w:?} {order:?}");
            compared += stored.len();
            let claims = |v: &View| -> Vec<Id> {
                all_pages(|c| idx.find_claims(w, v, order, 40, c).unwrap()).into_iter().map(|c: ClaimView| c.claim.id).collect()
            };
            assert_eq!(claims(&View::default()), claims(&computed_default()), "{ctx}: claims differ for {w:?} {order:?}");
            if order != Order::Relevance {
                continue; // the same entities again
            }
            for e in &stored {
                assert_eq!(
                    idx.entity(&e.id, &View::default()).unwrap(),
                    idx.entity(&e.id, &computed_default()).unwrap(),
                    "{ctx}: entity {} differs",
                    e.id
                );
            }
        }
    }
    compared
}

const A: usize = 1; // agent
const H: usize = 2; // human member

fn setup() -> (World, TempDir, crate::Index) {
    let mut w = World::new(3);
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

fn humans_accepted() -> View {
    View { status: vec![Status::Accepted], actor_kinds: vec![ActorKind::Human], ..View::default() }
}

fn agents() -> View {
    View { actor_kinds: vec![ActorKind::Agent], ..View::default() }
}

fn find(idx: &crate::Index, w: &Where, v: &View) -> Vec<EntityView> {
    idx.find_entities(w, v, Order::Relevance, 100, None).unwrap().items
}

fn ids(items: &[EntityView]) -> Vec<Id> {
    items.iter().map(|e| e.id.clone()).collect()
}

fn text_q(q: &str) -> Where {
    Where { text: Some(q.into()), ..Where::default() }
}

#[test]
fn excluded_claims_cannot_make_an_entity_match_or_relabel_it() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let typ = w.say(G, &e, vocab::TYPE, term(vocab::T_PERSON));
    let smear = w.label(A, &e, "Convicted fraudster");
    apply(&mut idx, &[&typ, &smear]);

    assert!(find(&idx, &text_q("fraudster"), &humans_accepted()).is_empty(), "the agent's label is not evidence in this view");
    assert_eq!(ids(&find(&idx, &text_q("fraudster"), &View::default())), vec![e.clone()]);

    let typed = Where { type_: vec![Term::parse(vocab::T_PERSON).unwrap()], ..Where::default() };
    let got = find(&idx, &typed, &humans_accepted());
    assert_eq!(ids(&got), vec![e.clone()]);
    assert_eq!(got[0].label, None, "display fields come from passing claims only");
    assert_eq!(got[0].claims, vec![typ.id.clone()]);

    let one = idx.entity(&e, &humans_accepted()).unwrap().unwrap();
    assert_eq!(one.label, None);
    assert_eq!(one.types, vec![Term::parse(vocab::T_PERSON).unwrap()]);
    assert_eq!(idx.entity(&e, &View::default()).unwrap().unwrap().label.as_deref(), Some("Convicted fraudster"));
}

#[test]
fn every_text_token_must_be_found_in_a_passing_claim() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let l = w.label(G, &e, "alpha");
    let b = w.say(A, &e, vocab::BODY, text("beta"));
    apply(&mut idx, &[&l, &b]);
    assert_eq!(ids(&find(&idx, &text_q("alpha beta"), &View::default())), vec![e.clone()], "tokens may come from different claims");
    assert!(find(&idx, &text_q("alpha beta"), &humans_accepted()).is_empty(), "\"beta\" is only in an excluded claim");
    assert_eq!(ids(&find(&idx, &text_q("alpha"), &humans_accepted())), vec![e.clone()]);
    assert!(find(&idx, &text_q("alpha"), &agents()).is_empty());
    assert_eq!(ids(&find(&idx, &text_q("beta"), &agents())), vec![e.clone()]);
}

#[test]
fn type_when_and_near_match_only_through_passing_claims() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let l = w.label(G, &e, "place");
    let when_c = w.say(A, &e, vocab::WHEN, when("1999"));
    let where_c = w.say(A, &e, vocab::WHERE, geo("35.68", "139.76"));
    let typ = w.say(A, &e, vocab::TYPE, term(vocab::T_PHOTO));
    apply(&mut idx, &[&l, &when_c, &where_c, &typ]);

    let in_1999 = Where { when: Some(WhenRange { from: Some("1999".into()), to: Some("1999".into()) }), ..Where::default() };
    let near = Where { near: Some(Near { lat: "35.68".into(), lon: "139.76".into(), km: "1".into() }), ..Where::default() };
    let photos = Where { type_: vec![Term::parse(vocab::T_PHOTO).unwrap()], ..Where::default() };
    for (q, what) in [(&in_1999, "when"), (&near, "near"), (&photos, "type")] {
        assert_eq!(ids(&find(&idx, q, &View::default())), vec![e.clone()], "{what}, default view");
        assert!(find(&idx, q, &humans_accepted()).is_empty(), "{what}: only the agent's claim supports it");
    }

    let v = idx.entity(&e, &humans_accepted()).unwrap().unwrap();
    assert!(v.types.is_empty() && v.where_.is_none());
    assert!(v.when.is_none(), "the agent's when does not pass, and record time dates only notes");
    assert_eq!(v.claims, vec![l.id.clone()]);
}

#[test]
fn display_values_and_the_recorded_fallback_follow_the_view() {
    let (mut w, _d, mut idx) = setup();
    let e = ent();
    let early = w.label(A, &e, "agent's, earliest");
    let owner = w.label(G, &e, "owner's");
    let agent_when = w.say(A, &e, vocab::WHEN, when("2001"));
    let note = w.say(G, &e, vocab::TYPE, term(vocab::T_NOTE));
    apply(&mut idx, &[&early, &owner, &agent_when, &note]);

    let default = idx.entity(&e, &View::default()).unwrap().unwrap();
    assert_eq!(default.label.as_deref(), Some("owner's"), "accepted before proposed");
    assert_eq!(default.when.as_ref().unwrap().value, "2001");

    let proposed = View { status: vec![Status::Proposed], ..View::default() };
    let p = idx.entity(&e, &proposed).unwrap().unwrap();
    assert_eq!(p.label.as_deref(), Some("agent's, earliest"), "latest among the passing claims");

    let humans = idx.entity(&e, &humans_accepted()).unwrap().unwrap();
    let when = humans.when.unwrap();
    assert_eq!(when.source, WhenSource::Recorded);
    assert_eq!(when.value, owner.body.at.format(), "earliest passing claim, not the agent's earlier one");
}

#[test]
fn text_blob_content_counts_as_text_of_the_claim_that_links_it() {
    let (mut w, _d, mut idx) = setup();
    let mut blobs = MemBlobs::default();
    let txt = blobs.put(b"secret plans");
    let note = ent();
    // the agent (an extractor, say) types the blob; the owner names it and links it
    let mt = w.say(A, &txt, vocab::MEDIA_TYPE, text("text/plain"));
    let name = w.label(G, &txt, "my file");
    let content = w.say(G, &note, vocab::CONTENT, Value::Ref(txt.clone()));
    apply_blobs(&mut idx, &blobs, &[&mt, &name, &content]);

    let mut both = vec![txt.clone(), note.clone()];
    both.sort();
    let mut got = ids(&find(&idx, &text_q("plans"), &View::default()));
    got.sort();
    assert_eq!(got, both);
    assert_eq!(ids(&find(&idx, &text_q("plans"), &humans_accepted())), vec![note.clone()], "the blob's own text hangs on the agent's mediaType claim");
    assert_eq!(ids(&find(&idx, &text_q("plans"), &agents())), vec![txt.clone()], "the note's text hangs on the owner's content claim");
}

#[test]
fn not_type_drops_entities_only_through_passing_type_claims() {
    let (mut w, _d, mut idx) = setup();
    let file = ent();
    let untyped = ent();
    let agent_typed = ent();
    let fl = w.label(G, &file, "report alpha");
    let ft = w.say(G, &file, vocab::TYPE, term(vocab::T_FILE));
    let ul = w.label(G, &untyped, "idea alpha");
    let al = w.label(G, &agent_typed, "scan alpha");
    let agent_type = w.say(A, &agent_typed, vocab::TYPE, term(vocab::T_FILE));
    // a claim whose subject is not an entity: never typed, so never dropped
    let review = w.review(G, &agent_type, true);
    apply(&mut idx, &[&fl, &ft, &ul, &al, &agent_type]);
    apply(&mut idx, &[&review]);

    let not_files = Where { not_type: vec![Term::parse(vocab::T_FILE).unwrap()], ..Where::default() };
    let sorted = |mut v: Vec<Id>| {
        v.sort();
        v
    };
    assert_eq!(ids(&find(&idx, &not_files, &View::default())), vec![untyped.clone()], "both files dropped, the untyped entity kept");
    assert!(find(&idx, &not_files, &agents()).is_empty(), "in this view agent_typed is typed and the others have no passing claim");
    assert_eq!(
        sorted(ids(&find(&idx, &not_files, &humans_accepted()))),
        sorted(vec![untyped.clone(), agent_typed.clone()]),
        "the agent's type claim does not pass, so it drops nothing"
    );
    let with_text = Where { text: Some("alpha".into()), ..not_files.clone() };
    assert_eq!(ids(&find(&idx, &with_text, &View::default())), vec![untyped.clone()]);

    let claims = |v: &View| -> Vec<Id> {
        idx.find_claims(&not_files, v, Order::Recorded, 100, None).unwrap().items.into_iter().map(|c| c.claim.id).collect()
    };
    let got = claims(&View::default());
    assert!(got.contains(&ul.id) && got.contains(&review.id), "{got:?}");
    assert!(![&fl.id, &ft.id, &al.id, &agent_type.id].iter().any(|c| got.contains(c)), "claims about a dropped entity go too: {got:?}");
    assert!(claims(&humans_accepted()).contains(&al.id));

    let typed = Where { type_: vec![Term::parse(vocab::T_FILE).unwrap()], ..not_files.clone() };
    assert!(find(&idx, &typed, &View::default()).is_empty(), "type and notType of the same term exclude each other");
    let compared = assert_routes_agree(&idx, &[not_files, with_text, typed], "notType");
    assert!(compared > 0);
}

/// The queries `assert_routes_agree` runs over the fixtures here and in the
/// equivalence test: every entity-level filter, alone and combined, plus claim filters.
pub(super) fn route_queries(owner: &Id) -> Vec<Where> {
    let t = |q: &str| Some(q.to_string());
    let typed = |ts: &[&str]| ts.iter().map(|x| Term::parse(x).unwrap()).collect::<Vec<_>>();
    let near = || Some(Near { lat: "35.5".into(), lon: "139.5".into(), km: "60".into() });
    let when = |f: &str, to: &str| Some(WhenRange { from: Some(f.into()), to: Some(to.into()) });
    vec![
        Where::default(),
        Where { text: t("alpha"), ..Where::default() },
        Where { text: t("alpha beta"), ..Where::default() },
        Where { text: t("東京"), ..Where::default() },
        Where { text: t("plans"), ..Where::default() },
        Where { type_: typed(&[vocab::T_PHOTO]), ..Where::default() },
        Where { type_: typed(&[vocab::T_NOTE, vocab::T_PERSON]), text: t("gamma"), ..Where::default() },
        Where { not_type: typed(&[vocab::T_PHOTO]), ..Where::default() },
        Where { not_type: typed(&[vocab::T_NOTE]), type_: typed(&[vocab::T_PHOTO, vocab::T_PERSON]), when: when("2010", "2030"), ..Where::default() },
        Where { when: when("2012", "2020"), ..Where::default() },
        Where { near: near(), ..Where::default() },
        Where { near: near(), when: when("2010", "2030"), ..Where::default() },
        Where { p: vec![Term::parse(vocab::LABEL).unwrap()], actors: vec![owner.clone()], ..Where::default() },
        Where { p: vec![Term::parse(vocab::BODY).unwrap()], text: t("delta"), ..Where::default() },
    ]
}

#[test]
fn stored_and_computed_routes_agree() {
    let (mut w, _d, mut idx) = setup();
    let mut blobs = MemBlobs::default();
    let txt = blobs.put("secret plans alpha".as_bytes());
    let words = ["alpha", "beta", "gamma", "delta", "東京タワー"];
    let types = [vocab::T_NOTE, vocab::T_PHOTO, vocab::T_PERSON];
    let mut claims = Vec::new();
    let mut earlier: Vec<knowa_model::Claim> = Vec::new();
    for i in 0..30usize {
        let e = ent();
        let who = [G, A, H][i % 3];
        claims.push(w.label(who, &e, &format!("{} {}", words[i % 5], words[(i * 7) % 5])));
        claims.push(w.say([A, G, H][i % 3], &e, vocab::BODY, text(words[(i * 3) % 5])));
        claims.push(w.say(who, &e, vocab::TYPE, term(types[i % 3])));
        if i % 2 == 0 {
            claims.push(w.say([H, A][(i / 2) % 2], &e, vocab::WHEN, when(&format!("20{}", 10 + i % 15))));
        }
        if i % 4 == 1 {
            claims.push(w.say(A, &e, vocab::WHEN, when("2016/..")));
        }
        if i % 3 != 2 {
            claims.push(w.say([G, A][i % 2], &e, vocab::WHERE, geo(&format!("35.{}", 10 + i), &format!("139.{}", 10 + i * 2))));
        }
        if i % 5 == 0 {
            claims.push(w.say(G, &e, vocab::CONTENT, Value::Ref(txt.clone())));
        }
        // statuses beyond accepted/proposed, and staleness
        if i % 6 == 3 {
            let target = claims[claims.len() - 2].clone();
            claims.push(w.review(G, &target, false));
        }
        if i % 7 == 4 {
            let target = claims[claims.len() - 1].clone();
            claims.push(w.retract(G, &target));
        }
        if i % 8 == 5
            && let Some(input) = earlier.last()
        {
            let input = input.id.clone();
            claims.push(w.with(A, &e, vocab::SUMMARY, text("derived alpha"), |d| d.src = vec![input]));
        }
        earlier.push(claims[claims.len() - 1].clone());
    }
    claims.push(w.say(A, &txt, vocab::MEDIA_TYPE, text("text/plain")));
    let refs: Vec<&knowa_model::Claim> = claims.iter().collect();
    apply_blobs(&mut idx, &blobs, &refs);

    let compared = assert_routes_agree(&idx, &route_queries(&w.owner()), "fixture");
    assert!(compared > 100, "only {compared} entity results compared");
}
