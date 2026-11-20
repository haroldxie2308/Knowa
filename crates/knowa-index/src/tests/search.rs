//! Full-text search: AND + last-token prefix, CJK substrings, FTS syntax in user
//! input, scores, and text blobs.

use knowa_model::query::{Order, View, Where};
use knowa_model::{ErrorCode, Id, Value, vocab};

use super::kit::*;

fn search(idx: &crate::Index, q: &str) -> Vec<Id> {
    let w = Where { text: Some(q.to_string()), ..Where::default() };
    idx.find_entities(&w, &View::default(), Order::Relevance, 100, None).unwrap().items.into_iter().map(|e| e.id).collect()
}

struct Fixture {
    w: World,
    _dir: TempDir,
    idx: crate::Index,
}

fn fixture(labels: &[&str]) -> (Fixture, Vec<Id>) {
    let mut w = World::new(2);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let g = w.genesis();
    apply(&mut idx, &[&g]);
    let mut ids = Vec::new();
    for l in labels {
        let e = ent();
        let c = w.label(G, &e, l);
        apply(&mut idx, &[&c]);
        ids.push(e);
    }
    (Fixture { w, _dir: dir, idx }, ids)
}

#[test]
fn all_words_must_match_and_only_the_last_is_a_prefix() {
    let (f, ids) = fixture(&["Quarterly budget review", "budget", "Review of the garden"]);
    assert_eq!(search(&f.idx, "budget"), {
        let mut v = vec![ids[0].clone(), ids[1].clone()];
        v.sort_by_key(|id| (id != &ids[1], id.clone())); // shorter doc ranks first
        v
    });
    assert_eq!(search(&f.idx, "budget rev"), vec![ids[0].clone()]);
    assert_eq!(search(&f.idx, "budg review"), Vec::<Id>::new(), "only the last word is a prefix");
    assert_eq!(search(&f.idx, "QUARTERLY"), vec![ids[0].clone()], "case-insensitive");
    assert_eq!(search(&f.idx, "garden zebra"), Vec::<Id>::new());
}

#[test]
fn scores_are_set_and_higher_is_better() {
    let (f, ids) = fixture(&["apple", "apple banana cherry date elderberry fig grape"]);
    let w = Where { text: Some("apple".into()), ..Where::default() };
    let items = f.idx.find_entities(&w, &View::default(), Order::Relevance, 10, None).unwrap().items;
    assert_eq!(items.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), ids);
    assert!(items[0].score.unwrap() > items[1].score.unwrap());
    let unscored = f.idx.find_entities(&Where::default(), &View::default(), Order::Relevance, 10, None).unwrap();
    assert!(unscored.items.iter().all(|e| e.score.is_none()));
}

#[test]
fn cjk_is_searchable_by_any_substring() {
    let (f, ids) = fixture(&["東京タワーの夜景", "北京大学图书馆", "서울특별시 강남구", "iPhone用ケース"]);
    for (q, want) in [
        ("京タ", 0),
        ("タワー", 0),
        ("夜景", 0),
        ("の夜", 0),
        ("大学图", 1),
        ("图书", 1),
        ("특별", 2),
        ("강남", 2),
        ("用ケ", 3),
        ("iphone", 3),
        ("iPhone用", 3),
    ] {
        assert_eq!(search(&f.idx, q), vec![ids[want].clone()], "query {q:?}");
    }
    assert!(search(&f.idx, "大阪").is_empty());
    assert!(search(&f.idx, "京图").is_empty(), "CJK runs match as contiguous phrases");
}

#[test]
fn fts_syntax_in_user_input_is_literal() {
    let (f, ids) = fixture(&["budget", "Notes about NOT gates", "near field"]);
    for q in [
        "budget\"", "budget OR zebra", "NEAR(budget review)", "body:budget", "budget*", "-budget", "(budget", "^budget",
        "budget AND", "* OR *", "{budget}", "budget + -", "'budget'",
    ] {
        let w = Where { text: Some(q.to_string()), ..Where::default() };
        let r = f.idx.find_entities(&w, &View::default(), Order::Relevance, 10, None);
        assert!(r.is_ok(), "query {q:?} failed: {:?}", r.err());
    }
    assert!(search(&f.idx, "budget OR zebra").is_empty(), "OR is a word, not an operator");
    assert_eq!(search(&f.idx, "NOT"), vec![ids[1].clone()], "NOT is a word, not an operator");
    assert_eq!(search(&f.idx, "NEAR"), vec![ids[2].clone()]);
    // as an FTS column filter this would match the "budget" document
    assert!(search(&f.idx, "body:budget").is_empty(), "`body` is a required word, not a column filter");

    let w = Where { text: Some("!!! ...".into()), ..Where::default() };
    let err = f.idx.find_entities(&w, &View::default(), Order::Relevance, 10, None).err().unwrap();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn only_accepted_and_proposed_claims_feed_the_document() {
    let (mut f, ids) = fixture(&["keepme"]);
    let c = f.w.label(G, &ids[0], "dropme");
    apply(&mut f.idx, &[&c]);
    assert_eq!(search(&f.idx, "dropme"), vec![ids[0].clone()]);
    let r = f.w.retract(G, &c);
    apply(&mut f.idx, &[&r]);
    assert!(search(&f.idx, "dropme").is_empty());
    assert_eq!(search(&f.idx, "keepme"), vec![ids[0].clone()]);
    let t = f.w.say(G, &ids[0], vocab::TYPE, term(vocab::T_PERSON));
    apply(&mut f.idx, &[&t]);
    assert_eq!(search(&f.idx, "person"), vec![ids[0].clone()], "type names are indexed");
}

#[test]
fn text_blobs_are_indexed_for_themselves_and_through_content() {
    let mut w = World::new(1);
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let mut blobs = MemBlobs::default();
    let g = w.genesis();
    let txt = blobs.put("the lazy fox jumps".as_bytes());
    let png = blobs.put(b"lazyimage bytes");
    let mut big = vec![b'x'; crate::MAX_FTS_BLOB_BYTES as usize];
    big.extend_from_slice(b" hugeword");
    let huge = blobs.put(&big);
    let note = ent();
    let content = w.say(G, &note, vocab::CONTENT, Value::Ref(txt.clone()));
    let label = w.label(G, &note, "a note");
    apply_blobs(&mut idx, &blobs, &[&g, &content, &label]);
    assert!(search(&idx, "lazy").is_empty(), "media type of the blob not known yet");

    let mt_txt = w.say(G, &txt, vocab::MEDIA_TYPE, text("text/plain"));
    let mt_png = w.say(G, &png, vocab::MEDIA_TYPE, text("image/png"));
    let mt_huge = w.say(G, &huge, vocab::MEDIA_TYPE, text("text/plain"));
    apply_blobs(&mut idx, &blobs, &[&mt_txt, &mt_png, &mt_huge]);
    let mut want = vec![txt.clone(), note.clone()];
    want.sort();
    let mut got = search(&idx, "lazy");
    got.sort();
    assert_eq!(got, want, "the blob itself and the entity whose content it is");
    assert!(search(&idx, "lazyimage").is_empty(), "non-text blobs are not indexed");
    assert!(search(&idx, "hugeword").is_empty(), "blobs over MAX_FTS_BLOB_BYTES are not indexed");
    assert_eq!(idx.media_type(&txt).unwrap().as_deref(), Some("text/plain"));
}
