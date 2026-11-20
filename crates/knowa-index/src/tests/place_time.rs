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
fn recorded_fallback_when_and_ordering() {
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
    let c_old = w.say(G, &old, vocab::WHEN, when("1999"));
    let c_new = w.say(G, &new, vocab::WHEN, when("2030"));
    apply(&mut idx, &[&g, &n2, &n1, &c_old, &c_new]);

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
