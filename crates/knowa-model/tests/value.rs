//! Values, terms and decimals (DESIGN §4.2, §4.3).

use knowa_model::value::{MAX_SAFE_INT, MAX_TEXT_BYTES};
use knowa_model::{Dec, ErrorCode, Term, Value};
use serde_json::{Value as Json, json};

fn invalid(v: &Json) -> String {
    match Value::from_json(v) {
        Ok(parsed) => panic!("{v} was accepted as {parsed:?}"),
        Err(e) => {
            assert_eq!(e.code, ErrorCode::Invalid, "{v}: {e}");
            e.message
        }
    }
}

#[test]
fn every_variant_round_trips() {
    for j in [
        json!({"text": "hello"}),
        json!({"text": ""}),
        json!({"text": "日本語", "lang": "ja-JP"}),
        json!({"term": "ex:Thing"}),
        json!({"ref": "e_01jdfq7a2b3c4d5e6f7g8h9j0k"}),
        json!({"ref": "b_ciqcz4snxjp3biyoe3udwkwfxhrj4gywdzob7j2clzzqim3csofzqja#page=2"}),
        json!({"ref": "x:wd:Q42"}),
        json!({"ref": "_:draft-only"}),
        json!({"int": 0}),
        json!({"int": MAX_SAFE_INT}),
        json!({"int": -MAX_SAFE_INT}),
        json!({"dec": "12.50"}),
        json!({"dec": "-0.001"}),
        json!({"dec": "007"}),
        json!({"bool": true}),
        json!({"when": "2024-05~/.."}),
        json!({"geo": {"lat": "35.68", "lon": "139.76"}}),
        json!({"geo": {"lat": "-90", "lon": "180.000", "alt": "-12.5", "r": "0"}}),
        json!({"qty": "12.5", "unit": "kg"}),
    ] {
        let v = Value::from_json(&j).unwrap_or_else(|e| panic!("{j}: {e}"));
        assert_eq!(v.to_json(), j);
        assert_eq!(serde_json::to_value(&v).unwrap(), j);
        assert_eq!(serde_json::from_value::<Value>(j.clone()).unwrap(), v);
    }
}

#[test]
fn shape_errors_name_the_offending_key() {
    assert!(invalid(&json!({"text": "a", "extra": 1})).contains("extra"));
    assert!(invalid(&json!({"int": 1, "lang": "en"})).contains("lang"));
    assert!(invalid(&json!({"text": "a", "unit": "kg"})).contains("unit"));
    let two = invalid(&json!({"text": "a", "term": "ex:A"}));
    assert!(two.contains("text") && two.contains("term"), "{two}");
    assert!(invalid(&json!({"geo": {"lat": "0", "lon": "0", "x": "1"}})).contains("`x`"));
    for not_a_value in [json!({}), json!([]), json!("text"), json!(42), json!(null), json!({"lang": "en"})] {
        invalid(&not_a_value);
    }
}

#[test]
fn text_limits() {
    let max = "a".repeat(MAX_TEXT_BYTES);
    assert!(Value::from_json(&json!({ "text": max })).is_ok());
    invalid(&json!({ "text": format!("{max}a") }));
    invalid(&json!({"text": 1}));
    invalid(&json!({"text": "a", "lang": ""}));
    invalid(&json!({"text": "a", "lang": "en_US"}));
    invalid(&json!({"text": "a", "lang": 5}));
}

#[test]
fn scalar_rejections() {
    for j in [
        json!({"term": "Thing"}),
        json!({"term": "Ex:Thing"}),
        json!({"term": "ex:1a"}),
        json!({"ref": "nope"}),
        json!({"ref": 42}),
        json!({"int": 1.0}),
        json!({"int": 1.5}),
        json!({"int": "1"}),
        json!({"int": MAX_SAFE_INT + 1}),
        json!({"int": -MAX_SAFE_INT - 1}),
        json!({"int": u64::MAX}),
        json!({"dec": 12.5}),
        json!({"bool": "true"}),
        json!({"bool": 1}),
        json!({"when": "2024-13"}),
        json!({"when": 2024}),
        json!({"qty": "1"}),
        json!({"qty": "1", "unit": ""}),
        json!({"qty": "1.", "unit": "kg"}),
        json!({"qty": 1, "unit": "kg"}),
    ] {
        invalid(&j);
    }
}

#[test]
fn geo_ranges_are_exact() {
    for j in [
        json!({"geo": {"lat": "90.0000001", "lon": "0"}}),
        json!({"geo": {"lat": "-90.5", "lon": "0"}}),
        json!({"geo": {"lat": "0", "lon": "180.0001"}}),
        json!({"geo": {"lat": "0", "lon": "-181"}}),
        json!({"geo": {"lat": "0", "lon": "0", "r": "-0.1"}}),
        json!({"geo": {"lat": "0"}}),
        json!({"geo": {"lon": "0"}}),
        json!({"geo": {"lat": 0, "lon": 0}}),
        json!({"geo": "35.68,139.76"}),
    ] {
        invalid(&j);
    }
    assert!(Value::from_json(&json!({"geo": {"lat": "90.000", "lon": "-180", "r": "-0"}})).is_ok());
}

#[test]
fn dec_grammar() {
    for ok in ["0", "-0", "12.50", "-3.14159", "007", "123456789012345678901234567890.5"] {
        assert_eq!(Dec::parse(ok).unwrap().as_str(), ok);
    }
    for bad in ["", "-", "1.", ".5", "+1", "1e3", "1,5", "--1", " 1", "1 ", "0x10", "١"] {
        assert_eq!(Dec::parse(bad).unwrap_err().code, ErrorCode::Invalid, "{bad:?}");
    }
    assert_eq!(Dec::parse("-0.25").unwrap().to_f64(), -0.25);
}

#[test]
fn term_grammar() {
    let t = Term::parse("ex-1:Name_v2.x-y").unwrap();
    assert_eq!((t.ns(), t.name()), ("ex-1", "Name_v2.x-y"));
    for bad in [
        "", "ex", ":Name", "ex:", "Ex:Name", "1ex:Name", "e_x:Name", "ex:1Name", "ex:_Name", "ex:a:b", "ex:a b", "ex:né",
    ] {
        assert_eq!(Term::parse(bad).unwrap_err().code, ErrorCode::Invalid, "{bad:?}");
    }
}
