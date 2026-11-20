//! Identifier grammar (DESIGN §4.1).

use knowa_model::{ErrorCode, Id, IdKind};

/// `b_` id of b"hello", `c_`/`a_` ids from vectors.json.
const BLOB: &str = "b_ciqcz4snxjp3biyoe3udwkwfxhrj4gywdzob7j2clzzqim3csofzqja";
const CLAIM: &str = "c_ciqeidtd3cejlxpgb6diitvk2odyajxwjqqrqjzlk4eelfemhvhknza";
const ACTOR: &str = "a_5uaqhiihx7z44ef6dvyn2ghhjpajsz7e2yyjxjinl4o5zbtecjktdoa";
const ENTITY: &str = "e_01jdfq7a2b3c4d5e6f7g8h9j0k";
const SPACE: &str = "s_01jdfq6tz3m8x9v2k4n5p7r8s0";

fn rejected(s: &str) -> bool {
    match Id::parse(s) {
        Ok(_) => false,
        Err(e) => {
            assert_eq!(e.code, ErrorCode::Invalid, "{s:?}: {e}");
            true
        }
    }
}

#[test]
fn accepts_every_kind() {
    for (s, kind) in [
        (CLAIM, IdKind::Claim),
        (BLOB, IdKind::Blob),
        (ACTOR, IdKind::Actor),
        (ENTITY, IdKind::Entity),
        (SPACE, IdKind::Space),
        ("e_00000000000000000000000000", IdKind::Entity),
        ("s_7zzzzzzzzzzzzzzzzzzzzzzzzz", IdKind::Space),
        ("x:wd:Q42", IdKind::External),
        ("x:url:https://example.com/a?b=c", IdKind::External),
        ("_:photo_1-a", IdKind::Placeholder),
    ] {
        let id = Id::parse(s).unwrap();
        assert_eq!(id.kind(), kind, "{s}");
        assert_eq!(id.as_str(), s);
    }
}

#[test]
fn hashed_ids_have_exactly_one_textual_form() {
    let body = &BLOB[2..];
    let mut trailing_bit = BLOB.to_string();
    // 55 chars carry 275 bits for 272: the last char's low 3 bits must be zero ('a' = 0).
    assert!(trailing_bit.ends_with('a'));
    trailing_bit.replace_range(56.., "b");
    for bad in [
        format!("b_{}", body.to_uppercase()),
        format!("b_{body}="),
        format!("b_{}", &body[..54]),
        format!("b_{body}a"),
        trailing_bit,
        format!("b_{}1", &body[..54]), // '1' is not base32
    ] {
        assert!(rejected(&bad), "{bad}");
    }
}

#[test]
fn hashed_ids_check_their_multihash_or_multicodec_header() {
    // same length and alphabet, wrong header for the kind
    assert!(rejected(&format!("c_{}", &ACTOR[2..])));
    assert!(rejected(&format!("b_{}", &ACTOR[2..])));
    assert!(rejected(&format!("a_{}", &CLAIM[2..])));
}

#[test]
fn ulids_are_lowercase_crockford_and_128_bits() {
    for bad in [
        "e_01jdfq7a2b3c4d5e6f7g8h9j0",   // 25
        "e_01jdfq7a2b3c4d5e6f7g8h9j0kk", // 27
        "e_01JDFQ7A2B3C4D5E6F7G8H9J0K",  // uppercase
        "e_01jdfq7a2b3c4d5e6f7g8h9j0i",  // i
        "e_01jdfq7a2b3c4d5e6f7g8h9j0l",  // l
        "e_01jdfq7a2b3c4d5e6f7g8h9j0o",  // o
        "e_01jdfq7a2b3c4d5e6f7g8h9j0u",  // u
        "s_8zzzzzzzzzzzzzzzzzzzzzzzzz",  // > 128 bits
    ] {
        assert!(rejected(bad), "{bad}");
    }
}

#[test]
fn external_and_placeholder_grammar() {
    for bad in [
        "x:", "x:wd", "x::Q42", "x:WD:Q42", "x:w_d:Q42", "x:wd:", "x:wd:Q 42", "x:wd:Q\t42", "x:wd:Q\u{a0}42", "_:", "_:a b",
        "_:a.b", "_:é", "_:a#b",
    ] {
        assert!(rejected(bad), "{bad:?}");
    }
}

#[test]
fn rejects_unknown_prefixes() {
    for bad in ["", "c", "q_abc", "C_abc", "e-01jdfq7a2b3c4d5e6f7g8h9j0k", "é_"] {
        assert!(rejected(bad), "{bad:?}");
    }
}

#[test]
fn blob_fragments() {
    for frag in ["xywh=0,0,1,1", "xywh=10,20,640,480", "t=3,9", "t=0,0.5", "t=1.25,2", "page=1", "page=12", "char=0,10"] {
        let id = Id::parse(&format!("{BLOB}#{frag}")).unwrap_or_else(|e| panic!("{frag}: {e}"));
        assert_eq!(id.kind(), IdKind::Blob);
        assert_eq!(id.fragment(), Some(frag));
        assert_eq!(id.base(), Id::parse(BLOB).unwrap());
    }
    for frag in [
        "",
        "page",
        "page=",
        "page=0",
        "page=01",
        "page=1,2",
        "xywh=0,0,0,1",
        "xywh=0,0,1,0",
        "xywh=1,2,3",
        "xywh=-1,0,1,1",
        "t=9,3",
        "t=3,3",
        "t=3",
        "t=1.50,2",
        "t=01,2",
        "t=.5,1",
        "t=1.,2",
        "char=5,5",
        "char=-1,3",
        "foo=1",
        "page=1#page=2",
        "page=99999999999999999999",
    ] {
        assert!(rejected(&format!("{BLOB}#{frag}")), "{frag:?}");
    }
}

#[test]
fn fragments_only_on_blobs() {
    for base in [CLAIM, ACTOR, ENTITY, SPACE, "_:a"] {
        assert!(rejected(&format!("{base}#page=1")), "{base}");
    }
    // An x: key is foreign and opaque: '#' belongs to it and is not a fragment.
    let url = Id::parse("x:url:https://example.com/doc#section").unwrap();
    assert_eq!(url.fragment(), None);
    assert_eq!(url.base(), url);
    let plain = Id::parse(BLOB).unwrap();
    assert_eq!(plain.fragment(), None);
    assert_eq!(plain.base(), plain);
}

#[test]
fn actor_pubkey_requires_an_actor_id() {
    let err = Id::parse(CLAIM).unwrap().actor_pubkey().unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn serde_uses_the_string_form_and_validates() {
    let id: Id = serde_json::from_str(&format!("\"{ENTITY}\"")).unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{ENTITY}\""));
    assert!(serde_json::from_str::<Id>("\"e_nope\"").is_err());
}

#[test]
fn minted_ulids_parse_and_are_unique() {
    use knowa_model::{Id, IdKind};
    let a = Id::new_entity().unwrap();
    let b = Id::new_entity().unwrap();
    let s = Id::new_space().unwrap();
    assert_eq!(Id::parse(a.as_str()).unwrap(), a);
    assert_eq!(Id::parse(s.as_str()).unwrap().kind(), IdKind::Space);
    assert_eq!(a.kind(), IdKind::Entity);
    assert_ne!(a, b);
    // ULID time prefix sorts by creation time (same or later ms).
    assert!(a.as_str()[2..12] <= b.as_str()[2..12]);
}
