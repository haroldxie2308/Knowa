//! DESIGN §4.7: text matching is normalization-insensitive. Indexed text (claim text
//! and text blobs) and query text are both NFKC-normalized before tokenizing; claims
//! and display fields keep the bytes as written.

use std::collections::BTreeSet;

use knowa_model::query::{Order, View, Where};
use knowa_model::{Claim, Id, Value, vocab};
use unicode_normalization::UnicodeNormalization;

use super::evidence::assert_routes_agree;
use super::kit::*;
use crate::fts;

/// The decomposed (NFD) spelling of composed text, as macOS file names have it. Fails
/// when `s` has none, so a test cannot quietly compare a string with itself.
fn nfd(s: &str) -> String {
    let d: String = s.nfd().collect();
    assert_ne!(d, s, "{s:?} has no decomposed form");
    assert!(d.chars().any(|c| ('\u{1100}'..='\u{11FF}').contains(&c)), "{s:?} decomposes into no Hangul jamo");
    d
}

fn search_in(idx: &crate::Index, view: &View, q: &str) -> Vec<Id> {
    let w = Where { text: Some(q.to_string()), ..Where::default() };
    let mut ids: Vec<Id> = idx.find_entities(&w, view, Order::Relevance, 100, None).unwrap().items.into_iter().map(|e| e.id).collect();
    ids.sort();
    ids
}

fn search(idx: &crate::Index, q: &str) -> Vec<Id> {
    search_in(idx, &View::default(), q)
}

fn sorted(mut ids: Vec<Id>) -> Vec<Id> {
    ids.sort();
    ids
}

fn setup(w: &mut World) -> (TempDir, crate::Index) {
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    (dir, idx)
}

#[test]
fn document_and_query_are_normalized_before_splitting() {
    assert_eq!(fts::document(&nfd("문법")), fts::document("문법"));
    assert_eq!(fts::document("문법"), " 문  법 ");
    assert_eq!(fts::document("ＴＯＰＩＫ 토픽Ⅱ ﬁle"), "TOPIK  토  픽 II file");

    assert_eq!(fts::match_terms(&nfd("문법")).unwrap(), fts::match_terms("문법").unwrap());
    assert_eq!(fts::match_terms("문법").unwrap(), vec!["\"문 법\"*"]);
    assert_eq!(fts::match_terms("ＴＯＰＩＫ").unwrap(), vec!["\"TOPIK\"*"]);
    assert_eq!(fts::match_terms("토픽Ⅱ").unwrap(), fts::match_terms("토픽 II").unwrap());
    // NFKC expands `⑴` into `(1)`: the parentheses are split off like any other
    // punctuation, so they never reach FTS syntax
    assert_eq!(fts::match_terms("⑴ topik").unwrap(), vec!["\"1\"", "\"topik\"*"]);
}

#[test]
fn nfd_and_nfc_names_find_each_other() {
    let mut w = World::new(1);
    let (_dir, mut idx) = setup(&mut w);
    let mut blobs = MemBlobs::default();
    let decomposed = blobs.put(b"%PDF-1.7 grammar");
    let composed = blobs.put(b"%PDF-1.7 past papers");
    let a = w.say(G, &decomposed, vocab::NAME, text(&nfd("한국어 문법_pdf.txt")));
    let b = w.say(G, &composed, vocab::NAME, text("토픽 기출문제.pdf"));
    apply_blobs(&mut idx, &blobs, &[&a, &b]);

    // the reported bug: a composed query against a decomposed file name
    assert_eq!(search(&idx, "문법"), vec![decomposed.clone()]);
    assert_eq!(search(&idx, "한국어 문법"), vec![decomposed.clone()]);
    assert_eq!(search(&idx, &nfd("문법")), vec![decomposed.clone()]);
    // and the other way round
    assert_eq!(search(&idx, &nfd("기출")), vec![composed.clone()]);
    assert_eq!(search(&idx, &nfd("토픽 기출문제")), vec![composed.clone()]);
    assert_eq!(search(&idx, "기출"), vec![composed.clone()]);
    assert!(search(&idx, "문제집").is_empty());
}

#[test]
fn nfd_and_nfc_text_blobs_find_each_other() {
    let mut w = World::new(1);
    let (_dir, mut idx) = setup(&mut w);
    let mut blobs = MemBlobs::default();
    let decomposed = blobs.put(nfd("듣기 평가 대본").as_bytes());
    let composed = blobs.put("읽기 영역 해설".as_bytes());
    let note = ent();
    let content = w.say(G, &note, vocab::CONTENT, Value::Ref(decomposed.clone()));
    let mt_d = w.say(G, &decomposed, vocab::MEDIA_TYPE, text("text/plain"));
    let mt_c = w.say(G, &composed, vocab::MEDIA_TYPE, text("text/plain"));
    apply_blobs(&mut idx, &blobs, &[&content, &mt_d, &mt_c]);

    // the blob itself (its mediaType claim) and the entity whose content it is
    let both = sorted(vec![decomposed.clone(), note.clone()]);
    assert_eq!(search(&idx, "평가"), both);
    assert_eq!(search(&idx, "평가 대본"), both);
    assert_eq!(search(&idx, &nfd("평가")), both);
    assert_eq!(search(&idx, &nfd("영역 해설")), vec![composed.clone()]);
    assert_eq!(search(&idx, "영역"), vec![composed.clone()]);
}

#[test]
fn compatibility_forms_match() {
    let mut w = World::new(1);
    let (_dir, mut idx) = setup(&mut w);
    let (wide, roman, liga) = (ent(), ent(), ent());
    let a = w.label(G, &wide, "ＴＯＰＩＫ 기출");
    let b = w.label(G, &roman, "토픽Ⅱ 문제집");
    let c = w.label(G, &liga, "ﬁnal review");
    apply(&mut idx, &[&a, &b, &c]);

    assert_eq!(search(&idx, "topik"), vec![wide.clone()]);
    assert_eq!(search(&idx, "TOPIK 기출"), vec![wide.clone()]);
    assert_eq!(search(&idx, "토픽 II"), vec![roman.clone()]);
    assert_eq!(search(&idx, "토픽 ii"), vec![roman.clone()]);
    assert_eq!(search(&idx, "final"), vec![liga.clone()]);
    // compatibility forms in the query
    let ascii = (ent(), ent());
    let d = w.label(G, &ascii.0, "TOPIK guide");
    let e = w.label(G, &ascii.1, "토픽II 문제집");
    apply(&mut idx, &[&d, &e]);
    assert_eq!(search(&idx, "ＴＯＰＩＫ"), sorted(vec![wide.clone(), ascii.0.clone()]));
    assert_eq!(search(&idx, "토픽Ⅱ"), sorted(vec![roman.clone(), ascii.1.clone()]));
    assert_eq!(search(&idx, "ﬁnal"), vec![liga.clone()]);
}

#[test]
fn display_keeps_the_bytes_as_written() {
    let mut w = World::new(1);
    let (_dir, mut idx) = setup(&mut w);
    let mut blobs = MemBlobs::default();
    let file = blobs.put(b"%PDF-1.7 grammar");
    let label = nfd("한국어 문법");
    let name = nfd("한국어 문법_pdf.txt");
    let e = ent();
    let wide = ent();
    let l = w.label(G, &e, &label);
    let n = w.say(G, &file, vocab::NAME, text(&name));
    let fw = w.label(G, &wide, "ＴＯＰＩＫ");
    apply_blobs(&mut idx, &blobs, &[&l, &n, &fw]);

    // stored route (default view) and computed route (any other view)
    for view in [View::default(), all_statuses()] {
        let shown = idx.entity(&e, &view).unwrap().unwrap();
        assert_eq!(shown.label.as_deref().map(str::as_bytes), Some(label.as_bytes()), "label under {view:?}");
        let found = idx.find_entities(&Where { text: Some("문법".into()), ..Where::default() }, &view, Order::Relevance, 10, None).unwrap();
        let hit = found.items.iter().find(|x| x.id == e).expect("the NFD label is found by an NFC query");
        assert_eq!(hit.label.as_deref().map(str::as_bytes), Some(label.as_bytes()));
        let fw_shown = idx.entity(&wide, &view).unwrap().unwrap();
        assert_eq!(fw_shown.label.as_deref(), Some("ＴＯＰＩＫ"), "full-width stays full-width");
    }

    let written = |c: &Claim| match &c.body.o {
        Value::Text { text, .. } => text.clone(),
        other => panic!("not text: {other:?}"),
    };
    for got in idx.claims(&[l.id.clone(), n.id.clone()]).unwrap() {
        let want = if got.claim.id == l.id { &label } else { &name };
        assert_eq!(written(&got.claim).as_bytes(), want.as_bytes(), "claim text as written");
        assert_eq!(got.claim, if got.claim.id == l.id { l.clone() } else { n.clone() });
    }
    let w_name = Where { text: Some("문법".into()), p: vec![knowa_model::Term::parse(vocab::NAME).unwrap()], ..Where::default() };
    let matched = idx.find_claims(&w_name, &View::default(), Order::Relevance, 10, None).unwrap().items;
    assert_eq!(matched.iter().map(|c| written(&c.claim)).collect::<Vec<_>>(), vec![name.clone()]);
}

/// Claims that reach the index through every text path with mixed spellings: an NFD
/// file name, labels in full-width and compatibility forms, an NFD text blob linked
/// by a content claim before its media type is known, whose text-ness then flips
/// off (retracted) and on again, and a retracted NFD label.
fn history(w: &mut World, blobs: &mut MemBlobs) -> (Vec<Claim>, [Id; 4]) {
    let pdf = blobs.put(b"%PDF-1.7 grammar");
    let script = blobs.put(format!("{} TOPIK", nfd("듣기 평가 대본")).as_bytes());
    let (note, book) = (ent(), ent());
    let mut out = vec![w.genesis()];
    out.push(w.say(G, &pdf, vocab::NAME, text(&nfd("한국어 문법_pdf.txt"))));
    out.push(w.say(G, &note, vocab::CONTENT, Value::Ref(script.clone())));
    out.push(w.label(G, &note, "ＴＯＰＩＫ 듣기"));
    out.push(w.label(G, &book, "토픽Ⅱ 문제집"));
    let mt = w.say(G, &script, vocab::MEDIA_TYPE, text("text/plain"));
    out.push(mt.clone());
    let old = w.label(G, &book, &nfd("문법 정리"));
    out.push(old.clone());
    out.push(w.retract(G, &old));
    out.push(w.retract(G, &mt));
    out.push(w.say(G, &script, vocab::MEDIA_TYPE, text("text/plain; charset=utf-8")));
    (out, [pdf, script, note, book])
}

#[test]
fn incremental_apply_equals_rebuild_across_spellings() {
    let mut w = World::new(1);
    let mut blobs = MemBlobs::default();
    let (claims, [pdf, script, note, book]) = history(&mut w, &mut blobs);

    // from scratch: one batch in log order, as a rebuild replays it
    let dir0 = TempDir::new();
    let mut reference = w.index(&dir0);
    reference.apply_with(&blobs, &claims).unwrap();
    // one claim at a time, in log order and in reverse (retracts before their
    // targets, the media type before the content claim)
    let dir1 = TempDir::new();
    let mut forward = w.index(&dir1);
    for c in &claims {
        forward.apply_with(&blobs, std::slice::from_ref(c)).unwrap();
    }
    let dir2 = TempDir::new();
    let mut backward = w.index(&dir2);
    for c in claims.iter().rev() {
        backward.apply_with(&blobs, std::slice::from_ref(c)).unwrap();
    }

    // each group: the same query in equivalent spellings
    let groups: Vec<Vec<String>> = vec![
        vec!["문법".into(), nfd("문법")],
        vec!["한국어 문법".into(), nfd("한국어 문법")],
        vec!["평가 대본".into(), nfd("평가 대본")],
        vec!["정리".into(), nfd("정리")],
        vec!["topik".into(), "ＴＯＰＩＫ".into(), "TOPIK".into()],
        vec!["토픽 II".into(), "토픽Ⅱ".into(), "토픽 ⅱ".into()],
    ];
    let results = |idx: &crate::Index, view: &View, q: &str| -> Vec<(Id, Option<String>, i64)> {
        let w = Where { text: Some(q.to_string()), ..Where::default() };
        idx.find_entities(&w, view, Order::Relevance, 100, None)
            .unwrap()
            .items
            .into_iter()
            // bm25 over identical documents is identical; rounding only guards
            // against float noise in the last bits
            .map(|e| (e.id, e.label, (e.score.unwrap() * 1e9).round() as i64))
            .collect()
    };
    for view in [View::default(), all_statuses()] {
        for group in &groups {
            let want = results(&reference, &view, &group[0]);
            for q in group {
                assert_eq!(results(&reference, &view, q), want, "{q:?} vs {:?} under {view:?}", group[0]);
                assert_eq!(results(&forward, &view, q), want, "{q:?}: incremental (log order) vs rebuild under {view:?}");
                assert_eq!(results(&backward, &view, q), want, "{q:?}: incremental (reverse) vs rebuild under {view:?}");
            }
        }
    }

    // the history is not vacuous: every path is hit, and the view still decides
    assert_eq!(search(&reference, &nfd("문법")), vec![pdf.clone()], "the NFD label was retracted");
    assert_eq!(search_in(&reference, &all_statuses(), "문법"), sorted(vec![pdf.clone(), book.clone()]));
    assert_eq!(search(&reference, "평가 대본"), sorted(vec![script.clone(), note.clone()]));
    assert_eq!(search(&reference, "ＴＯＰＩＫ"), sorted(vec![script.clone(), note.clone()]));
    assert_eq!(search(&reference, "토픽 II"), vec![book.clone()]);
    assert!(search(&reference, "정리").is_empty());

    let wheres: Vec<Where> = groups.iter().flatten().map(|q| Where { text: Some(q.clone()), ..Where::default() }).collect();
    let compared = assert_routes_agree(&reference, &wheres, "normalization history");
    assert!(compared > 0, "no entity results compared across routes");
    let distinct: BTreeSet<Id> = groups.iter().flatten().flat_map(|q| search(&reference, q)).collect();
    assert_eq!(distinct.len(), 4);
}
