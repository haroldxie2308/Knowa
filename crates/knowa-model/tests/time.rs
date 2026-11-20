//! Record time and the EDTF subset (DESIGN §4.4, §4.5). Expected instants are written
//! as canonical timestamps, which `vectors.json` pins against an independent oracle.

use knowa_model::time::parse_duration_ms;
use knowa_model::{ErrorCode, Timestamp, When};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn ts(s: &str) -> i64 {
    Timestamp::parse(s).unwrap_or_else(|e| panic!("{s}: {e}")).0
}

fn day(date: &str) -> i64 {
    ts(&format!("{date}T00:00:00.000Z"))
}

fn when(s: &str) -> When {
    When::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn assert_invalid<T: std::fmt::Debug>(input: &str, r: knowa_model::Result<T>) {
    match r {
        Ok(v) => panic!("{input:?} was accepted as {v:?}"),
        Err(e) => assert_eq!(e.code, ErrorCode::Invalid, "{input:?}: {e}"),
    }
}

#[test]
fn date_forms_cover_their_whole_period() {
    for (s, lo, hi) in [
        ("2024", day("2024-01-01"), day("2025-01-01")),
        ("0000", day("0000-01-01"), day("0001-01-01")),
        ("2024-02", day("2024-02-01"), day("2024-03-01")),
        ("2023-02", day("2023-02-01"), day("2023-03-01")),
        ("2024-12", day("2024-12-01"), day("2025-01-01")),
        ("2024-02-29", day("2024-02-29"), day("2024-03-01")),
        ("9999-12-31", day("9999-12-31"), day("9999-12-31") + DAY),
        ("199X", day("1990-01-01"), day("2000-01-01")),
        ("19XX", day("1900-01-01"), day("2000-01-01")),
    ] {
        let w = when(s);
        assert_eq!((w.lo, w.hi), (Some(lo), Some(hi)), "{s}");
        assert_eq!((w.uncertain, w.approximate, w.local_unknown), (false, false, false), "{s}");
        assert_eq!(w.raw, s);
    }
}

#[test]
fn datetimes_with_offset_are_one_millisecond_instants() {
    for (s, instant) in [
        ("2024-05-01T10:00:00+09:00", "2024-05-01T01:00:00.000Z"),
        ("2024-05-01T10:00Z", "2024-05-01T10:00:00.000Z"),
        ("2024-05-01T10:00:30.5Z", "2024-05-01T10:00:30.500Z"),
        ("2024-05-01T10:00:30.123-05:30", "2024-05-01T15:30:30.123Z"),
        ("2024-05-01T23:30-01:00", "2024-05-02T00:30:00.000Z"),
        ("2024-05-01T00:00+00:00", "2024-05-01T00:00:00.000Z"),
    ] {
        let w = when(s);
        let t = ts(instant);
        assert_eq!((w.lo, w.hi), (Some(t), Some(t + 1)), "{s}");
        assert!(!w.local_unknown, "{s}");
    }
}

#[test]
fn datetimes_without_offset_widen_by_14_hours() {
    let w = when("2024-05-01T10:00");
    let t = ts("2024-05-01T10:00:00.000Z");
    assert_eq!((w.lo, w.hi), (Some(t - 14 * HOUR), Some(t + 14 * HOUR + 1)));
    assert!(w.local_unknown);
    assert_eq!(w.center(), Some(t));
}

#[test]
fn qualifiers_set_flags_without_widening() {
    let plain = when("2024-05");
    for (s, uncertain, approximate) in [("2024-05?", true, false), ("2024-05~", false, true), ("2024-05%", true, true)] {
        let w = when(s);
        assert_eq!((w.lo, w.hi), (plain.lo, plain.hi), "{s}");
        assert_eq!((w.uncertain, w.approximate), (uncertain, approximate), "{s}");
    }
    assert!(when("2024?").uncertain);
    assert!(when("2024-05-01~").approximate);
}

#[test]
fn intervals() {
    let w = when("2020/2022-06");
    assert_eq!((w.lo, w.hi), (Some(day("2020-01-01")), Some(day("2022-07-01"))));

    let w = when("2020/..");
    assert_eq!((w.lo, w.hi), (Some(day("2020-01-01")), None));
    assert_eq!(w.center(), None);

    let w = when("../2020-06");
    assert_eq!((w.lo, w.hi), (None, Some(day("2020-07-01"))));
    assert_eq!(w.center(), None);

    let w = when("2020/2020");
    assert_eq!((w.lo, w.hi), (Some(day("2020-01-01")), Some(day("2021-01-01"))));

    let w = when("2024-05-01T10:00Z/2024-05-01T12:00Z");
    assert_eq!((w.lo, w.hi), (Some(ts("2024-05-01T10:00:00.000Z")), Some(ts("2024-05-01T12:00:00.000Z") + 1)));

    // flags from either side
    let w = when("2020~/2022?");
    assert_eq!((w.uncertain, w.approximate, w.local_unknown), (true, true, false));
    assert_eq!(w.raw, "2020~/2022?");
    assert!(when("2024-05-01T10:00/2024-05-02").local_unknown);
    assert!(when("../2024-05-01T10:00").local_unknown);
}

#[test]
fn center_is_the_midpoint() {
    assert_eq!(when("2024-05-01").center(), Some(ts("2024-05-01T12:00:00.000Z")));
    // a hand-built range whose sum overflows i64 still has a midpoint
    let wide = When { lo: Some(i64::MAX - 2), hi: Some(i64::MAX), ..when("2024") };
    assert_eq!(wide.center(), Some(i64::MAX - 1));
}

#[test]
fn rejects_everything_outside_the_subset() {
    for s in [
        "",
        "24",
        "20245",
        "10000",
        "-2024",
        "+2024",
        "Y12345",
        "2024-5",
        "2024-13",
        "2024-00",
        "2024-21", // EDTF season
        "2024-W01",
        "2024-123",
        "2023-02-29",
        "2024-04-31",
        "2024-05-00",
        "2024-05-01T24:00Z",
        "2024-05-01T10:60Z",
        "2024-05-01T10:00:60Z",
        "2024-05-01T10Z",
        "2024-05-01T10",
        "2024-05-01 10:00Z",
        "2024-05-01t10:00Z",
        "2024-05-01T10:00z",
        "2024-05-01T10:00:00.Z",
        "2024-05-01T10:00:00.1234Z",
        "2024-05-01T10:00:00,5Z",
        "2024-05-01T10:00.5Z",
        "2024-05-01T10:00+0900",
        "2024-05-01T10:00+09",
        "2024-05-01T10:00+24:00",
        "2024-05-01T10:00+09:60",
        "2024-05-01T10:00Z?",
        "2024-05-01T10:00~",
        "199X?",
        "19XX~",
        "1XXX",
        "XXXX",
        "199x",
        "19X5",
        "2024-XX",
        "2024??",
        "?",
        "~2024",
        "2024/2023",
        "2022-06/2022",
        "2024-05-01T12:00Z/2024-05-01T10:00Z",
        "../..",
        "2020/",
        "/2020",
        "2020/2021/2022",
        "2020/../2021",
        " 2024",
        "2024 ",
    ] {
        assert_invalid(s, When::parse(s));
    }
}

#[test]
fn timestamp_parse_accepts_only_the_canonical_form() {
    for s in [
        "2026-11-21T10:00:00Z",
        "2026-11-21T10:00:00.12Z",
        "2026-11-21T10:00:00.1234Z",
        "2026-11-21T10:00:00.123+00:00",
        "2026-11-21t10:00:00.123Z",
        "2026-11-21T10:00:00.123z",
        "2026-11-21 10:00:00.123Z",
        "2026-02-30T00:00:00.000Z",
        "2026-11-21T23:59:60.000Z",
        "2026-11-21T24:00:00.000Z",
        " 2026-11-21T10:00:00.123Z",
        "+2026-11-21T10:00:00.123Z",
        "+10000-01-01T00:00:00.000Z",
    ] {
        assert_invalid(s, Timestamp::parse(s));
    }
}

#[test]
fn timestamp_format_outside_years_0000_to_9999_uses_expanded_years() {
    // reachable: the end of EDTF `9999`, and year 0000 shifted by an offset
    let end = when("9999").hi.unwrap();
    assert_eq!(Timestamp(end).format(), "+10000-01-01T00:00:00.000Z");
    let before = when("0000-01-01T00:00+01:00").lo.unwrap();
    assert_eq!(Timestamp(before).format(), "-0001-12-31T23:00:00.000Z");
}

#[test]
fn rfc3339_is_lenient_about_precision_offset_and_case() {
    for (s, canonical) in [
        ("2026-11-21T10:00:00Z", "2026-11-21T10:00:00.000Z"),
        ("2026-11-21t10:00:00z", "2026-11-21T10:00:00.000Z"),
        ("2026-11-21T19:00:00+09:00", "2026-11-21T10:00:00.000Z"),
        ("2026-11-21T10:00:00.123456789Z", "2026-11-21T10:00:00.123Z"),
        ("2026-11-21T10:00:00.9-00:30", "2026-11-21T10:30:00.900Z"),
        ("1969-12-31T23:59:59.9999Z", "1969-12-31T23:59:59.999Z"),
        ("2026-11-21T10:00:00.123Z", "2026-11-21T10:00:00.123Z"),
    ] {
        assert_eq!(Timestamp::parse_rfc3339(s).unwrap_or_else(|e| panic!("{s}: {e}")).format(), canonical, "{s}");
    }
    for s in [
        "2026-11-21",
        "2026-11-21T10:00Z",
        "2026-11-21T10:00:00",
        "2026-11-21T10:00:00.Z",
        "2026-11-21 10:00:00Z",
        "2026-11-21T10:00:60Z",
        "2026-11-21T10:00:00+0900",
    ] {
        assert_invalid(s, Timestamp::parse_rfc3339(s));
    }
}

#[test]
fn durations() {
    for (s, ms) in [
        ("P1D", DAY),
        ("P0D", 0),
        ("PT6H", 6 * HOUR),
        ("PT30M", 30 * 60_000),
        ("PT45S", 45_000),
        ("P1DT12H30M", DAY + 12 * HOUR + 30 * 60_000),
        ("PT1H1M1S", HOUR + 61_000),
        ("P2W", 14 * DAY),
    ] {
        assert_eq!(parse_duration_ms(s).unwrap_or_else(|e| panic!("{s}: {e}")), ms, "{s}");
    }
    for s in [
        "", "P", "PT", "P1DT", "1D", "p1d", "P1Y", "P1M", "P1Y2D", "PT1.5H", "P1.5D", "P-1D", "PT1H1H", "PT1M1H", "P1D1D",
        "P1W1D", "P1WT1H", "PT1W", "P1H", "PT99999999999999999999H", "P999999999999999W",
    ] {
        assert_invalid(s, parse_duration_ms(s));
    }
}

#[test]
fn now_is_canonical() {
    let now = Timestamp::now();
    assert_eq!(Timestamp::parse(&now.format()).unwrap(), now);
}
