//! Record time (`Timestamp`) and world time (`When`, EDTF subset — DESIGN §4.5).
//! Civil-date arithmetic is implemented here; no date crate.

use crate::error::Result;

/// Milliseconds since the Unix epoch, UTC. Serialized as `2006-01-02T15:04:05.000Z`
/// (exactly millisecond precision, `Z` suffix) — the only accepted form for `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Timestamp {
        todo!()
    }

    /// Strict: only the canonical form above.
    pub fn parse(s: &str) -> Result<Timestamp> {
        let _ = s;
        todo!()
    }

    /// Lenient RFC 3339 (any fractional precision, any offset) → UTC ms.
    /// For user-supplied filters like `recordedAfter`.
    pub fn parse_rfc3339(s: &str) -> Result<Timestamp> {
        let _ = s;
        todo!()
    }

    pub fn format(&self) -> String {
        todo!()
    }
}

/// A parsed EDTF value. `lo`/`hi` are a half-open range `[lo, hi)` in Unix ms;
/// `None` means open (`..`). `raw` is preserved exactly as written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct When {
    pub raw: String,
    pub lo: Option<i64>,
    pub hi: Option<i64>,
    /// `?` or `%` present
    pub uncertain: bool,
    /// `~` or `%` present
    pub approximate: bool,
    /// a datetime without offset: range widened by ±14h
    pub local_unknown: bool,
}

impl When {
    /// Accepts exactly the subset in DESIGN §4.5; anything else is `invalid`.
    /// Examples → [lo, hi):
    /// - `2024` → [2024-01-01, 2025-01-01)
    /// - `2024-05` → [2024-05-01, 2024-06-01)
    /// - `199X` → [1990-01-01, 2000-01-01)
    /// - `2024-05-01T10:00:00+09:00` → [01:00:00.000Z, 01:00:00.001Z) (instant = 1 ms)
    /// - `2024-05-01T10:00` (no offset) → [10:00Z − 14h, 10:00Z + 14h + 1ms), local_unknown
    /// - `2020/2022-06` → [2020-01-01, 2022-07-01)
    /// - `2020/..` → [2020-01-01, None)
    /// For intervals, qualifiers on either side set the flags.
    pub fn parse(s: &str) -> Result<When> {
        let _ = s;
        todo!()
    }

    /// Midpoint used for ordering/distance; `None` if either end is open.
    pub fn center(&self) -> Option<i64> {
        todo!()
    }
}

/// ISO 8601 duration subset for `window`: `PnD`, `PTnH`, `PTnM`, `PnDTnH…`, `PnW`.
/// Years/months are rejected (ambiguous length) → `invalid`.
pub fn parse_duration_ms(s: &str) -> Result<i64> {
    let _ = s;
    todo!()
}
