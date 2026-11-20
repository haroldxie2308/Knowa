//! Conformance suite: golden vectors produced by an independent Python oracle
//! (`vectors.json`). A change to the vectors is a format change.

use knowa_model::canonical::canonical_json;
use knowa_model::{Claim, ClaimBody, Draft, ErrorCode, Id, Signer, Timestamp};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

fn vectors() -> Json {
    serde_json::from_str(include_str!("vectors.json")).expect("vectors.json is valid JSON")
}

fn s(v: &Json) -> &str {
    v.as_str().expect("string field in vectors.json")
}

fn hex(v: &Json) -> Vec<u8> {
    data_encoding::HEXLOWER.decode(s(v).as_bytes()).expect("hex field in vectors.json")
}

#[test]
fn jcs_canonical_form() {
    for case in vectors()["jcs"].as_array().unwrap() {
        let input: Json = serde_json::from_str(s(&case["input"])).unwrap();
        let out = canonical_json(&input).unwrap_or_else(|e| panic!("{}: {e}", s(&case["name"])));
        assert_eq!(String::from_utf8(out).unwrap(), s(&case["canonical"]), "{}", s(&case["name"]));
    }
}

#[test]
fn jcs_rejects_floats_unsafe_integers_and_non_ascii_keys() {
    for case in vectors()["jcs_reject"].as_array().unwrap() {
        let input: Json = serde_json::from_str(s(&case["input"])).unwrap();
        let err = canonical_json(&input).expect_err(s(&case["name"]));
        assert_eq!(err.code, ErrorCode::Invalid, "{}", s(&case["name"]));
    }
}

#[test]
fn blob_ids() {
    for case in vectors()["blob"].as_array().unwrap() {
        let bytes = hex(&case["bytes_hex"]);
        let expected = Id::parse(s(&case["id"])).unwrap();
        assert_eq!(Id::blob_of(&bytes), expected);
        assert_eq!(Id::blob_from_sha256(&Sha256::digest(&bytes).into()), expected);
    }
}

#[test]
fn actor_id_from_seed() {
    let actor = &vectors()["actor"];
    let seed: [u8; 32] = hex(&actor["seed_hex"]).try_into().unwrap();
    let pubkey: [u8; 32] = hex(&actor["pubkey_hex"]).try_into().unwrap();
    let id = Signer::from_seed(seed).actor_id();
    assert_eq!(id.as_str(), s(&actor["id"]));
    assert_eq!(Id::actor_of(&pubkey), id);
    assert_eq!(id.actor_pubkey().unwrap(), pubkey);
}

#[test]
fn claims_canonical_bytes_id_sig_and_line() {
    let v = vectors();
    let seed: [u8; 32] = hex(&v["actor"]["seed_hex"]).try_into().unwrap();
    let signer = Signer::from_seed(seed);
    for case in v["claims"].as_array().unwrap() {
        let line = s(&case["line"]);

        // read path: a stored log line parses, verifies, and re-serializes byte-identically
        let stored = Claim::from_json(&serde_json::from_str(line).unwrap()).unwrap();
        stored.verify().unwrap();
        assert_eq!(String::from_utf8(stored.body.canonical_bytes().unwrap()).unwrap(), s(&case["canonical"]));
        assert_eq!(stored.id.as_str(), s(&case["id"]));
        assert_eq!(stored.sig, s(&case["sig"]));
        assert_eq!(String::from_utf8(stored.to_line().unwrap()).unwrap(), line);
        assert_eq!(stored.body.to_json(), case["body"]);

        // write path: the draft part plus the envelope seals to the same claim
        let body = &case["body"];
        let mut draft_json = body.as_object().unwrap().clone();
        for envelope in ["v", "space", "actor", "seq", "prev", "at"] {
            draft_json.remove(envelope);
        }
        let draft = Draft::from_json(&Json::Object(draft_json)).unwrap();
        assert_eq!(draft.equiv_key().unwrap(), s(&case["equiv_key"]));
        let sealed = signer
            .seal(ClaimBody::from_draft(
                draft,
                Id::parse(s(&body["space"])).unwrap(),
                Id::parse(s(&body["actor"])).unwrap(),
                body["seq"].as_u64().unwrap(),
                body.get("prev").map(|p| Id::parse(s(p)).unwrap()),
                Timestamp::parse(s(&body["at"])).unwrap(),
            ))
            .unwrap();
        assert_eq!(sealed, stored);
        assert_eq!(sealed.body.equiv_key().unwrap(), s(&case["equiv_key"]));
    }
}

#[test]
fn timestamps() {
    for case in vectors()["timestamps"].as_array().unwrap() {
        let ms = case["ms"].as_i64().unwrap();
        assert_eq!(Timestamp::parse(s(&case["text"])).unwrap(), Timestamp(ms));
        assert_eq!(Timestamp(ms).format(), s(&case["text"]));
    }
}
