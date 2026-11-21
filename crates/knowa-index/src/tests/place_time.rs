//! `near` (R*-tree prefilter + exact haversine) and `when` (half-open overlap).

use knowa_model::query::{Near, Order, View, Where, WhenRange};
use knowa_model::view::WhenSource;
use knowa_model::{ErrorCode, Id, vocab};

use super::kit::*;

fn near(lat: &str, lon: &str, km: &str) -> Where {
    Where { near: Some(Near { lat: lat.into(), lon: lon.into(), km: km.into() }), ..Where::default() }
}

fn ids(idx: &crate::Index, w: &Where, order: Order) -> Vec<Id> {
    idx.find_entities(w, &View::default(), order, 100, None).unwrap().items.into_iter().map(|e| e.id).collect()
}

fn placed(points: &[(&str, &str)]) -> (World, TempDir, crate::Index, Vec<Id>) {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    let mut out = Vec::new();
    for (lat, lon) in points {
        let e = ent();
        let c = w.say(G, &e, vocab::WHERE, geo(lat, lon));
        apply(&mut idx, &[&c]);
        out.push(e);
    }
    (w, dir, idx, out)
}

#[test]
fn near_filters_by_exact_distance_and_orders_by_it() {
    // Tokyo Station, Tokyo Tower (~3 km away), Osaka
    let (_w, _d, idx, e) = placed(&[("35.681236", "139.767125"), ("35.658581", "139.745433"), ("34.693738", "135.502165")]);
    assert_eq!(ids(&idx, &near("35.681236", "139.767125", "1"), Order::Distance), vec![e[0].clone()]);
    assert_eq!(ids(&idx, &near("35.681236", "139.767125", "5"), Order::Distance), vec![e[0].clone(), e[1].clone()]);
    assert_eq!(ids(&idx, &near("35.658581", "139.745433", "5"), Order::Distance), vec![e[1].clone(), e[0].clone()]);
    let items = idx.find_entities(&near("35.681236", "139.767125", "5"), &View::default(), Order::Distance, 10, None).unwrap().items;
    assert_eq!(items[0].distance_km, Some(0.0));
    let d = items[1].distance_km.unwrap();
    assert!((2.9..3.4).contains(&d), "Tokyo Station → Tokyo Tower is ~3.1 km, got {d}");
    // without `near`, no distance
    assert!(idx.find_entities(&Where::default(), &View::default(), Order::Recorded, 10, None).unwrap().items.iter().all(|e| e.distance_km.is_none()));
}

#[test]
fn bbox_corners_are_excluded_by_the_exact_check() {
    // 10 km around (0, 0): (0.06, 0.06) is ~9.4 km away, (0.08, 0.08) is ~12.6 km —
    // inside the bounding box but outside the circle.
    let (_w, _d, idx, e) = placed(&[("0.06", "0.06"), ("0.08", "0.08")]);
    assert_eq!(ids(&idx, &near("0", "0", "10"), Order::Distance), vec![e[0].clone()]);
}

#[test]
fn near_works_across_the_antimeridian_and_at_the_poles() {
    let (_w, _d, idx, e) = placed(&[("0", "-179.99"), ("89.99", "10"), ("0", "0")]);
    assert_eq!(ids(&idx, &near("0", "179.99", "5"), Order::Distance), vec![e[0].clone()]);
    assert_eq!(ids(&idx, &near("89.99", "-170", "5"), Order::Distance), vec![e[1].clone()]);
}

#[test]
fn near_input_is_validated() {
    let (w, _d, idx, _) = placed(&[]);
    for bad in [near("91", "0", "1"), near("0", "181", "1"), near("0", "0", "-1"), near("x", "0", "1")] {
        let err = idx.find_entities(&bad, &View::default(), Order::Distance, 10, None).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
    }
    let unresolved = Where {
        near_id: Some(knowa_model::query::NearId { id: w.space.clone(), km: "1".into() }),
        ..Where::default()
    };
    assert_eq!(idx.find_entities(&unresolved, &View::default(), Order::Relevance, 10, None).err().unwrap().code, ErrorCode::Invalid);
    assert_eq!(idx.find_claims(&unresolved, &View::default(), Order::Relevance, 10, None).err().unwrap().code, ErrorCode::Invalid);
    let err = idx.find_entities(&Where::default(), &View::default(), Order::Distance, 10, None).err().unwrap();
    assert_eq!(err.code, ErrorCode::Invalid, "order distance requires near");
}

fn when_filter(from: Option<&str>, to: Option<&str>) -> Where {
    Where { when: Some(WhenRange { from: from.map(String::from), to: to.map(String::from) }), ..Where::default() }
}

#[test]
fn when_filter_is_half_open_overlap_from_lo_to_hi() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let y2024 = ent();
    let open = ent();
    let c1 = w.say(G, &y2024, vocab::WHEN, when("2024"));
    let c2 = w.say(G, &open, vocab::WHEN, when("2026/.."));
    apply(&mut idx, &[&g, &c1, &c2]);
    let only = |f, t| ids(&idx, &when_filter(f, t), Order::When);
    assert_eq!(only(Some("2025"), None), vec![open.clone()], "[2024, 2025) does not overlap [2025, …)");
    assert_eq!(only(None, Some("2024")), vec![y2024.clone()], "`to` uses its hi: 2025-01-01");
    assert_eq!(only(None, Some("2023")), Vec::<Id>::new(), "[…, 2024-01-01) does not overlap [2024, 2025)");
    assert_eq!(only(Some("2024-12-31"), Some("2024-12-31")), vec![y2024.clone()]);
    assert_eq!(only(Some("2030"), Some("2031")), vec![open.clone()], "open-ended ranges are unbounded");
    assert_eq!(only(None, None), vec![y2024.clone(), open.clone()]);
    let err = idx.find_entities(&when_filter(Some("yesterday"), None), &View::default(), Order::When, 10, None).err().unwrap();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn reversed_when_range_is_invalid() {
    let (_w, _d, idx, _) = placed(&[]);
    for (from, to) in [("2025", "2024"), ("2024-06-02", "2024-06-01"), ("2030/..", "../2029")] {
        let err = idx.find_entities(&when_filter(Some(from), Some(to)), &View::default(), Order::When, 10, None).err();
        assert_eq!(err.map(|e| e.code), Some(ErrorCode::Invalid), "entities: {from} > {to}");
        let err = idx.find_claims(&when_filter(Some(from), Some(to)), &View::default(), Order::When, 10, None).err();
        assert_eq!(err.map(|e| e.code), Some(ErrorCode::Invalid), "claims: {from} > {to}");
    }
    for (from, to) in [("2024", "2024"), ("2024-06", "2024"), ("2024-12-31", "2024-12-31")] {
        assert!(idx.find_entities(&when_filter(Some(from), Some(to)), &View::default(), Order::When, 10, None).is_ok(), "{from}..{to}");
    }
}

#[test]
fn recorded_fallback_dates_a_note_and_orders_it() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let note = ent();
    let old = ent();
    let new = ent();
    let at = w.clock;
    let n1 = w.label(G, &note, "a note");
    let n2 = w.label(G, &note, "renamed note");
    let typ = w.say(G, &note, vocab::TYPE, term(vocab::T_NOTE));
    let c_old = w.say(G, &old, vocab::WHEN, when("1999"));
    let c_new = w.say(G, &new, vocab::WHEN, when("2030"));
    apply(&mut idx, &[&g, &n2, &n1, &typ, &c_old, &c_new]);

    let e = idx.entity(&note, &View::default()).unwrap().unwrap();
    let when = e.when.unwrap();
    assert_eq!(when.source, WhenSource::Recorded);
    assert_eq!(when.value, knowa_model::Timestamp(at).format(), "earliest record time");
    assert_eq!(e.label.as_deref(), Some("renamed note"), "latest accepted label is displayed");
    assert!(e.claims.contains(&n1.id) && e.claims.contains(&n2.id));

    assert_eq!(ids(&idx, &Where::default(), Order::When), vec![old.clone(), note.clone(), new.clone()]);
    assert_eq!(ids(&idx, &Where::default(), Order::WhenDesc), vec![new.clone(), note.clone(), old.clone()]);
    assert_eq!(ids(&idx, &Where::default(), Order::Relevance), vec![new, note, old], "relevance without text = -when");
}

// ---- §4.8 rules 3 and 4: record time dates notes only ----

#[test]
fn record_time_dates_a_note_and_no_other_entity() {
    const AGENT: usize = 1;
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    // what an agent creates while reading, with no stated time: a concept, a person
    let concept = ent();
    let person = ent();
    let c1 = w.label(AGENT, &concept, "Project Phoenix");
    let c2 = w.say(AGENT, &concept, vocab::SUMMARY, text("a codename from the minutes"));
    let p1 = w.say(AGENT, &person, vocab::TYPE, term(vocab::T_PERSON));
    let p2 = w.label(AGENT, &person, "Ada");
    let note = ent();
    let note_at = w.clock;
    let n1 = w.say(AGENT, &note, vocab::BODY, text("call Ada about Phoenix"));
    let n2 = w.say(AGENT, &note, vocab::TYPE, term(vocab::T_NOTE));
    apply(&mut idx, &[&g, &c1, &c2, &p1, &p2, &n1, &n2]);
    assert_eq!(status(&idx, &n2), knowa_model::Status::Proposed);

    let shown = |id: &Id| idx.entity(id, &View::default()).unwrap().unwrap();
    assert_eq!(shown(&concept).when, None, "untyped");
    assert_eq!(shown(&person).when, None, "a person is not a note");
    let n = shown(&note);
    let when = n.when.unwrap();
    assert_eq!((when.source, when.value), (WhenSource::Recorded, knowa_model::Timestamp(note_at).format()));
    assert!(n.claims.contains(&n1.id), "the earliest claim supports when");

    // every claim above was recorded in 2023
    let in_2023 = when_filter(Some("2023"), Some("2023"));
    assert_eq!(ids(&idx, &in_2023, Order::When), vec![note.clone()], "only the note is on the timeline");
    assert_eq!(ids(&idx, &Where::default(), Order::When)[0], note, "undated entities sort after dated ones");
    super::evidence::assert_routes_agree(&idx, &[Where::default(), in_2023], "record time dates notes only");
}

#[test]
fn a_note_type_that_does_not_pass_the_view_does_not_date_the_entity() {
    const AGENT: usize = 1;
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let e = ent();
    let body = w.say(G, &e, vocab::BODY, text("met Ada"));
    let typ = w.say(AGENT, &e, vocab::TYPE, term(vocab::T_NOTE));
    apply(&mut idx, &[&g, &body, &typ]);

    let when = idx.entity(&e, &View::default()).unwrap().unwrap().when.unwrap();
    assert_eq!((when.source, when.value), (WhenSource::Recorded, body.body.at.format()));
    let owner_only = View { actors: vec![w.owner()], ..View::default() };
    assert_eq!(idx.entity(&e, &owner_only).unwrap().unwrap().when, None, "in this view it is not a note");
    let in_2023 = when_filter(Some("2023"), Some("2023"));
    assert!(idx.find_entities(&in_2023, &owner_only, Order::When, 10, None).unwrap().items.is_empty());
}

// ---- §4.8 rule 2: the file time (knowa:modified) ----

#[test]
fn a_when_claim_beats_a_file_time_and_record_time_does_not_date_a_file() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let blob = MemBlobs::default().put(b"report.pdf bytes");
    let name = w.say(G, &blob, vocab::NAME, text("report.pdf"));
    apply(&mut idx, &[&g, &name]);
    let shown = |idx: &crate::Index, id: &Id| idx.entity(id, &View::default()).unwrap().unwrap();
    assert_eq!(shown(&idx, &blob).when, None, "a file is not a note: when it was added is not when it was made");

    let modified = w.say(G, &blob, vocab::MODIFIED, when("2014-05-13T16:53:20Z"));
    apply(&mut idx, &[&modified]);
    let e = shown(&idx, &blob);
    let file = e.when.unwrap();
    assert_eq!((file.source, file.value.as_str()), (WhenSource::File, "2014-05-13T16:53:20Z"));
    assert_eq!(file.from.as_deref(), Some("2014-05-13T16:53:20.000Z"));
    assert_eq!(file.to.as_deref(), Some("2014-05-13T16:53:20.001Z"));
    assert!(e.claims.contains(&modified.id), "the chosen knowa:modified supports when");

    let claimed = w.say(G, &blob, vocab::WHEN, when("2019-06"));
    apply(&mut idx, &[&claimed]);
    let e = shown(&idx, &blob);
    let both = e.when.unwrap();
    assert_eq!((both.source, both.value.as_str()), (WhenSource::Claim, "2019-06"));
    assert!(!e.claims.contains(&modified.id), "an unused knowa:modified supports nothing");

    // knowa:created never dates an entity
    let other = MemBlobs::default().put(b"other bytes");
    let created = w.say(G, &other, vocab::CREATED, when("2001-01-01T00:00:00Z"));
    apply(&mut idx, &[&created]);
    assert_eq!(shown(&idx, &other).when, None);
}

#[test]
fn the_earliest_of_several_file_times_dates_the_entity_and_orders_it() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let blob = MemBlobs::default().put(b"same bytes, three paths");
    let middle = w.say(G, &blob, vocab::MODIFIED, when("2016-01-01T00:00:00Z"));
    let earliest = w.say(G, &blob, vocab::MODIFIED, when("2014-01-01T00:00:00Z"));
    let latest = w.say(G, &blob, vocab::MODIFIED, when("2020-01-01T00:00:00Z"));
    let between = ent();
    let between_when = w.say(G, &between, vocab::WHEN, when("2015"));
    apply(&mut idx, &[&g, &middle, &earliest, &latest, &between_when]);

    let e = idx.entity(&blob, &View::default()).unwrap().unwrap();
    let file = e.when.unwrap();
    assert_eq!((file.source, file.value.as_str()), (WhenSource::File, "2014-01-01T00:00:00Z"));
    assert!(e.claims.contains(&earliest.id) && !e.claims.contains(&middle.id) && !e.claims.contains(&latest.id));
    assert_eq!(ids(&idx, &Where::default(), Order::When), vec![blob.clone(), between.clone()]);
    assert_eq!(ids(&idx, &when_filter(Some("2014"), Some("2014")), Order::When), vec![blob.clone()]);
    assert!(ids(&idx, &when_filter(Some("2016"), Some("2020")), Order::When).is_empty(), "only the chosen file time places the entity");
    super::evidence::assert_routes_agree(&idx, &[Where::default(), when_filter(Some("2014"), Some("2016"))], "file times");
}

#[test]
fn an_accepted_file_time_beats_an_earlier_proposed_one() {
    const STRANGER: usize = 1; // no role: its claims are proposed
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let blob = MemBlobs::default().put(b"contested bytes");
    let proposed = w.say(STRANGER, &blob, vocab::MODIFIED, when("1990-01-01T00:00:00Z"));
    let accepted = w.say(G, &blob, vocab::MODIFIED, when("2010-01-01T00:00:00Z"));
    apply(&mut idx, &[&g, &proposed, &accepted]);
    assert_eq!(status(&idx, &proposed), knowa_model::Status::Proposed);

    let file = idx.entity(&blob, &View::default()).unwrap().unwrap().when.unwrap();
    assert_eq!((file.source, file.value.as_str()), (WhenSource::File, "2010-01-01T00:00:00Z"));
    super::evidence::assert_routes_agree(&idx, &[Where::default()], "accepted file time");

    // with the accepted one retracted, the proposed one is the only candidate left
    let gone = w.retract(G, &accepted);
    apply(&mut idx, &[&gone]);
    let file = idx.entity(&blob, &View::default()).unwrap().unwrap().when.unwrap();
    assert_eq!((file.source, file.value.as_str()), (WhenSource::File, "1990-01-01T00:00:00Z"));
}

#[test]
fn a_view_excluding_the_file_times_author_leaves_the_file_undated() {
    const AGENT: usize = 1;
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    let blob = MemBlobs::default().put(b"agent-dated bytes");
    let name = w.say(G, &blob, vocab::NAME, text("scan.tiff"));
    let modified = w.say(AGENT, &blob, vocab::MODIFIED, when("2001-01-01T00:00:00Z"));
    apply(&mut idx, &[&g, &name, &modified]);

    assert_eq!(idx.entity(&blob, &View::default()).unwrap().unwrap().when.unwrap().source, WhenSource::File);
    let owner_only = View { actors: vec![w.owner()], ..View::default() };
    let e = idx.entity(&blob, &owner_only).unwrap().unwrap();
    assert_eq!(e.when, None, "no file time passes, and record time dates only notes");
    assert_eq!(e.claims, vec![name.id.clone()], "the name, as the label");
    let in_2001 = when_filter(Some("2001"), Some("2001"));
    assert!(idx.find_entities(&in_2001, &owner_only, Order::When, 10, None).unwrap().items.is_empty());
    assert_eq!(ids(&idx, &in_2001, Order::When), vec![blob]);
}
