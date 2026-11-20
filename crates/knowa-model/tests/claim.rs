//! Drafts, claims, sealing and verification (DESIGN §4.4).

use std::collections::BTreeMap;

use knowa_model::claim::MAX_NOTE_BYTES;
use knowa_model::{Claim, ClaimBody, Dec, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value, When};
use serde_json::{Value as Json, json};

const SPACE: &str = "s_01jdfq6tz3m8x9v2k4n5p7r8s0";
const ENTITY: &str = "e_01jdfq7a2b3c4d5e6f7g8h9j0k";
const OTHER_ENTITY: &str = "e_01jdfq7a2b3c4d5e6f7g8h9j0m";
const PREV: &str = "c_ciqeidtd3cejlxpgb6diitvk2odyajxwjqqrqjzlk4eelfemhvhknza";
const BLOB: &str = "b_ciqcz4snxjp3biyoe3udwkwfxhrj4gywdzob7j2clzzqim3csofzqja";

/// An in-place change to a claim body, for table-driven cases.
type BodyEdit = fn(&mut ClaimBody);

fn id(s: &str) -> Id {
    Id::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn term(s: &str) -> Term {
    Term::parse(s).unwrap()
}

fn signer() -> Signer {
    Signer::from_seed([7; 32])
}

/// A draft that uses every optional field.
fn rich_draft() -> Draft {
    Draft::from_json(&json!({
        "s": ENTITY,
        "p": "ex:note",
        "o": {"text": "hi", "lang": "en"},
        "q": {"ex:tag": [{"term": "ex:A"}, {"ref": OTHER_ENTITY}]},
        "valid": "2024-05",
        "conf": "0.5",
        "src": [format!("{BLOB}#page=1")],
        "replaces": [PREV],
        "via": {"tool": "test/1", "worker": "w@1"},
        "note": "because"
    }))
    .unwrap()
}

fn body(by: &Signer, draft: Draft) -> ClaimBody {
    let at = Timestamp::parse("2026-11-21T10:00:00.000Z").unwrap();
    ClaimBody::from_draft(draft, id(SPACE), by.actor_id(), 2, Some(id(PREV)), at)
}

fn sealed() -> Claim {
    let s = signer();
    s.seal(body(&s, rich_draft())).unwrap()
}

fn draft_err(v: Json) -> knowa_model::Error {
    match Draft::from_json(&v) {
        Ok(d) => panic!("{v} was accepted as {d:?}"),
        Err(e) => e,
    }
}

fn claim_err(v: Json) -> knowa_model::Error {
    match Claim::from_json(&v) {
        Ok(c) => panic!("{v} was accepted as {c:?}"),
        Err(e) => e,
    }
}

fn with(base: &Json, key: &str, value: Json) -> Json {
    let mut v = base.clone();
    v[key] = value;
    v
}

fn without(base: &Json, key: &str) -> Json {
    let mut v = base.clone();
    v.as_object_mut().unwrap().remove(key);
    v
}

#[test]
fn seal_then_verify_and_round_trip() {
    let claim = sealed();
    claim.verify().unwrap();
    assert_eq!(Claim::from_json(&claim.to_json()).unwrap(), claim);

    let line = claim.to_line().unwrap();
    assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
    assert!(line.ends_with(b"\n"));
    assert_eq!(Claim::from_json(&serde_json::from_slice(&line).unwrap()).unwrap(), claim);

    // genesis shape: seq 1 and no prev
    let s = signer();
    let at = Timestamp::parse("2026-11-21T10:00:00.000Z").unwrap();
    let genesis = ClaimBody::from_draft(
        Draft::new(id(SPACE), term("knowa:owner"), Value::Ref(s.actor_id())),
        id(SPACE),
        s.actor_id(),
        1,
        None,
        at,
    );
    s.seal(genesis).unwrap().verify().unwrap();
}

#[test]
fn changing_any_field_fails_verification_with_integrity() {
    let mutations: [(&str, BodyEdit); 15] = [
        ("space", |b| b.space = id("s_01jdfq6tz3m8x9v2k4n5p7r8s1")),
        ("actor", |b| b.actor = Signer::from_seed([8; 32]).actor_id()),
        ("seq", |b| b.seq = 3),
        ("prev", |b| b.prev = Some(Id::claim_of(b"another"))),
        ("at", |b| b.at = Timestamp(b.at.0 + 1)),
        ("s", |b| b.s = id(OTHER_ENTITY)),
        ("p", |b| b.p = term("ex:other")),
        ("o", |b| b.o = Value::text("hi!")),
        ("q", |b| {
            b.q.insert(term("ex:more"), vec![Value::Bool(true)]);
        }),
        ("valid", |b| b.valid = Some(When::parse("2024-06").unwrap())),
        ("conf", |b| b.conf = Some(Dec::parse("0.6").unwrap())),
        ("src", |b| b.src.push(id(BLOB))),
        ("replaces", |b| b.replaces.clear()),
        ("via", |b| {
            b.via.insert("model".into(), "m".into());
        }),
        ("note", |b| b.note = None),
    ];
    for (field, mutate) in mutations {
        let mut claim = sealed();
        mutate(&mut claim.body);
        // the stored id no longer matches the content
        assert_eq!(claim.verify().unwrap_err().code, ErrorCode::Integrity, "{field}");
        // with the id recomputed, only the signature can catch it
        claim.id = Id::claim_of(&claim.body.canonical_bytes().unwrap());
        assert_eq!(claim.verify().unwrap_err().code, ErrorCode::Integrity, "{field}, id recomputed");
    }

    // the same through the read path: a tampered log line still parses, then fails verify
    let mut line = sealed().to_json();
    line["note"] = json!("edited later");
    let parsed = Claim::from_json(&line).unwrap();
    assert_eq!(parsed.verify().unwrap_err().code, ErrorCode::Integrity);
}

#[test]
fn foreign_or_garbled_signatures_fail_with_integrity() {
    let claim = sealed();
    let other = Signer::from_seed([8; 32]);
    let mut by_other = claim.body.clone();
    by_other.actor = other.actor_id();
    let foreign_sig = other.seal(by_other).unwrap().sig;

    let mut flipped = claim.sig.clone();
    flipped.replace_range(..1, if flipped.starts_with('A') { "B" } else { "A" });

    let truncated = claim.sig[..80].to_string();
    let extended = format!("{}AA", claim.sig);
    for sig in [foreign_sig, flipped, truncated, extended, String::new(), "!!!".into()] {
        let tampered = Claim { sig: sig.clone(), ..claim.clone() };
        assert_eq!(tampered.verify().unwrap_err().code, ErrorCode::Integrity, "{sig:?}");
    }
    let wrong_id = Claim { id: Id::claim_of(b"x"), ..claim.clone() };
    assert_eq!(wrong_id.verify().unwrap_err().code, ErrorCode::Integrity);
}

#[test]
fn seal_refuses_a_body_for_another_actor() {
    let other = Signer::from_seed([8; 32]);
    let err = signer().seal(body(&other, rich_draft())).unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains(other.actor_id().as_str()), "{}", err.message);
}

#[test]
fn seal_validates_the_envelope_and_content() {
    let s = signer();
    let cases: [(&str, BodyEdit, ErrorCode); 16] = [
        ("seq 0", |b| b.seq = 0, ErrorCode::Invalid),
        ("seq beyond 2^53 - 1", |b| b.seq = 1 << 53, ErrorCode::Invalid),
        ("seq 2 without prev", |b| b.prev = None, ErrorCode::Invalid),
        ("seq 1 with prev", |b| b.seq = 1, ErrorCode::Invalid),
        ("prev not a claim", |b| b.prev = Some(id(ENTITY)), ErrorCode::Invalid),
        ("space not a space", |b| b.space = id(ENTITY), ErrorCode::Invalid),
        ("actor not an actor", |b| b.actor = id(ENTITY), ErrorCode::Invalid),
        ("at after 9999", |b| b.at = Timestamp(253_402_300_800_000), ErrorCode::Invalid),
        ("placeholder s", |b| b.s = id("_:x"), ErrorCode::Invalid),
        ("placeholder o", |b| b.o = Value::Ref(id("_:x")), ErrorCode::Invalid),
        ("placeholder src", |b| b.src = vec![id("_:x")], ErrorCode::Invalid),
        ("placeholder q", |b| b.q = BTreeMap::from([(term("ex:q"), vec![Value::Ref(id("_:x"))])]), ErrorCode::Invalid),
        ("empty q values", |b| b.q = BTreeMap::from([(term("ex:q"), vec![])]), ErrorCode::Invalid),
        (
            "When fields disagree with raw",
            |b| b.valid = Some(When { hi: None, ..When::parse("2024").unwrap() }),
            ErrorCode::Invalid,
        ),
        ("int beyond 2^53 - 1", |b| b.o = Value::Int(1 << 53), ErrorCode::Invalid),
        ("claim version 2", |b| b.v = 2, ErrorCode::Unsupported),
    ];
    for (name, mutate, code) in cases {
        let mut b = body(&s, rich_draft());
        mutate(&mut b);
        assert_eq!(s.seal(b.clone()).unwrap_err().code, code, "{name}");
        // verify rejects the same structure before looking at id or signature
        let claim = Claim { body: b, id: id(PREV), sig: sealed().sig };
        assert_eq!(claim.verify().unwrap_err().code, code, "{name}");
    }
}

#[test]
fn equiv_key_covers_s_p_o_q_valid_only() {
    let s = signer();
    let draft = rich_draft();
    let key = draft.equiv_key().unwrap();
    assert_eq!(s.seal(body(&s, draft.clone())).unwrap().body.equiv_key().unwrap(), key);

    let mut metadata_only = draft.clone();
    metadata_only.conf = None;
    metadata_only.src.clear();
    metadata_only.replaces.clear();
    metadata_only.via.clear();
    metadata_only.note = None;
    assert_eq!(metadata_only.equiv_key().unwrap(), key);

    let changes: [fn(&mut Draft); 6] = [
        |d| d.s = id(OTHER_ENTITY),
        |d| d.p = term("ex:other"),
        |d| d.o = Value::text("other"),
        |d| d.q.clear(),
        |d| d.valid = None,
        |d| d.valid = Some(When::parse("2024-05~").unwrap()),
    ];
    for (i, change) in changes.into_iter().enumerate() {
        let mut d = draft.clone();
        change(&mut d);
        assert_ne!(d.equiv_key().unwrap(), key, "change #{i}");
    }
}

#[test]
fn equiv_key_requires_resolved_placeholders() {
    for d in [
        json!({"s": "_:a", "p": "ex:p", "o": {"int": 1}}),
        json!({"s": ENTITY, "p": "ex:p", "o": {"ref": "_:a"}}),
        json!({"s": ENTITY, "p": "ex:p", "o": {"int": 1}, "q": {"ex:q": [{"ref": "_:a"}]}}),
    ] {
        let draft = Draft::from_json(&d).unwrap();
        assert_eq!(draft.equiv_key().unwrap_err().code, ErrorCode::Invalid, "{d}");
    }
}

#[test]
fn placeholders_resolve_to_entities() {
    let e = |c: char| id(&format!("e_01jdfq7a2b3c4d5e6f7g8h9j0{c}"));
    let draft = Draft::from_json(&json!({
        "s": "_:a",
        "p": "ex:link",
        "o": {"ref": "_:b"},
        "q": {"ex:x": [{"ref": "_:a"}, {"ref": "_:c"}, {"int": 1}]},
        "src": ["_:d", BLOB, "_:b"]
    }))
    .unwrap();
    assert_eq!(draft.placeholders(), ["_:a", "_:b", "_:c", "_:d"].map(id));

    let mut map: BTreeMap<Id, Id> =
        [("_:a", 'a'), ("_:b", 'b'), ("_:c", 'c'), ("_:d", 'd')].into_iter().map(|(p, c)| (id(p), e(c))).collect();
    let resolved = draft.resolve(&map).unwrap();
    assert!(resolved.placeholders().is_empty());
    assert_eq!(resolved.s, e('a'));
    assert_eq!(resolved.o, Value::Ref(e('b')));
    assert_eq!(resolved.q[&term("ex:x")], vec![Value::Ref(e('a')), Value::Ref(e('c')), Value::Int(1)]);
    assert_eq!(resolved.src, vec![e('d'), id(BLOB), e('b')]);
    assert_eq!(resolved.p, draft.p);

    let s = signer();
    let at = Timestamp::parse("2026-11-21T10:00:00.000Z").unwrap();
    s.seal(ClaimBody::from_draft(resolved, id(SPACE), s.actor_id(), 1, None, at)).unwrap().verify().unwrap();

    map.remove(&id("_:d"));
    assert_eq!(draft.resolve(&map).unwrap_err().code, ErrorCode::Invalid);
}

#[test]
fn draft_round_trips_through_json() {
    let draft = rich_draft();
    assert_eq!(Draft::from_json(&draft.to_json()).unwrap(), draft);
    let minimal = Draft::new(id(ENTITY), term("ex:p"), Value::Bool(false));
    assert_eq!(minimal.to_json(), json!({"s": ENTITY, "p": "ex:p", "o": {"bool": false}}));
    assert_eq!(Draft::from_json(&minimal.to_json()).unwrap(), minimal);
}

#[test]
fn draft_from_json_rejects_envelope_unknown_and_missing_fields() {
    let base = json!({"s": ENTITY, "p": "ex:p", "o": {"int": 1}});
    for key in ["v", "space", "actor", "seq", "prev", "at", "id", "sig", "foo", "status"] {
        let e = draft_err(with(&base, key, json!(1)));
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.contains(&format!("`{key}`")), "{key}: {}", e.message);
    }
    for key in ["s", "p", "o"] {
        let e = draft_err(without(&base, key));
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.contains(&format!("`{key}`")), "{key}: {}", e.message);
    }
    draft_err(json!([]));
    draft_err(json!("draft"));
}

#[test]
fn draft_from_json_rejects_empty_containers_and_bad_fields() {
    let base = json!({"s": ENTITY, "p": "ex:p", "o": {"int": 1}});
    for (key, value) in [
        ("q", json!({})),
        ("q", json!({"ex:a": []})),
        ("q", json!({"notaterm": [{"int": 1}]})),
        ("q", json!({"ex:a": {"int": 1}})),
        ("src", json!([])),
        ("src", json!(BLOB)),
        ("src", json!(["nope"])),
        ("replaces", json!([])),
        ("replaces", json!([ENTITY])),
        ("replaces", json!(["_:a"])),
        ("via", json!({})),
        ("via", json!({"Tool": "x"})),
        ("via", json!({"1x": "x"})),
        ("via", json!({"": "x"})),
        ("via", json!({"tool": 1})),
        ("note", json!("")),
        ("note", json!("a".repeat(MAX_NOTE_BYTES + 1))),
        ("note", json!(1)),
        ("conf", json!("1.01")),
        ("conf", json!("-0.1")),
        ("conf", json!("abc")),
        ("conf", json!(0.5)),
        ("valid", json!("2024-13")),
        ("valid", json!(2024)),
        ("s", json!(1)),
        ("s", json!("nope")),
        ("p", json!("notaterm")),
        ("o", json!("text")),
    ] {
        let e = draft_err(with(&base, key, value.clone()));
        assert_eq!(e.code, ErrorCode::Invalid, "{key}={value}");
    }
    for (key, value) in [
        ("conf", json!("0")),
        ("conf", json!("1")),
        ("conf", json!("1.000")),
        ("note", json!("a".repeat(MAX_NOTE_BYTES))),
        ("via", json!({"a_1": ""})),
    ] {
        Draft::from_json(&with(&base, key, value.clone())).unwrap_or_else(|e| panic!("{key}={value}: {e}"));
    }
}

#[test]
fn claim_from_json_checks_version_first_then_shape() {
    let good = sealed().to_json();
    assert_eq!(claim_err(with(&good, "v", json!(2))).code, ErrorCode::Unsupported);
    // a future version may add fields: still `unsupported`, not `invalid`
    assert_eq!(claim_err(with(&with(&good, "v", json!(2)), "future", json!(true))).code, ErrorCode::Unsupported);

    for bad in [
        without(&good, "v"),
        with(&good, "v", json!("1")),
        with(&good, "v", json!(1.0)),
        with(&good, "foo", json!(1)),
        with(&good, "status", json!("accepted")),
        without(&good, "id"),
        without(&good, "sig"),
        without(&good, "at"),
        without(&good, "seq"),
        without(&good, "space"),
        without(&good, "actor"),
        without(&good, "s"),
        with(&good, "id", json!(ENTITY)),
        with(&good, "id", json!("c_bad")),
        with(&good, "sig", json!("AAAA")),
        with(&good, "sig", json!(format!("{}==", good["sig"].as_str().unwrap()))),
        with(&good, "sig", json!(format!("+{}", &good["sig"].as_str().unwrap()[1..]))), // standard, not url, alphabet
        with(&good, "at", json!("2026-11-21T10:00:00Z")),
        with(&good, "seq", json!(-1)),
        with(&good, "seq", json!(2.0)),
        with(&good, "seq", json!("2")),
        with(&good, "seq", json!(1)), // prev present with seq 1
        with(&good, "src", json!([])),
        json!([]),
    ] {
        assert_eq!(claim_err(bad.clone()).code, ErrorCode::Invalid, "{bad}");
    }
}

#[test]
fn generated_signers_are_distinct_and_reproducible() {
    let (a, b) = (Signer::generate().unwrap(), Signer::generate().unwrap());
    assert_ne!(a.actor_id(), b.actor_id());
    assert_eq!(Signer::from_seed(*a.seed()).actor_id(), a.actor_id());
}
