//! Record time (`Timestamp`) and world time (`When`, EDTF subset — DESIGN §4.5).
//! Civil-date arithmetic is implemented here; no date crate.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

const MS_PER_SEC: i64 = 1_000;
const MS_PER_MIN: i64 = 60 * MS_PER_SEC;
const MS_PER_HOUR: i64 = 60 * MS_PER_MIN;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;
/// Widening for a datetime without offset: real UTC offsets span −12:00…+14:00.
const LOCAL_UNKNOWN_MS: i64 = 14 * MS_PER_HOUR;

/// Milliseconds since the Unix epoch, UTC. Serialized as `2006-01-02T15:04:05.000Z`
/// (exactly millisecond precision, `Z` suffix) — the only accepted form for `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Timestamp {
        // `Duration` tops out near 1.8e22 ms, far inside i128, so these casts are lossless.
        let ms: i128 = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_millis() as i128,
            // A clock set before 1970 is still a valid instant: floor to the millisecond.
            Err(e) => {
                let before = e.duration();
                -(before.as_millis() as i128) - i128::from(before.subsec_nanos() % 1_000_000 != 0)
            }
        };
        // Invariant: a wall clock is never ±292 million years from 1970 (kernels cap it
        // far earlier, e.g. Linux at 2262), so the i64 millisecond count cannot overflow.
        Timestamp(i64::try_from(ms).expect("system clock within ±292 million years of 1970"))
    }

    /// Strict: only the canonical form above.
    pub fn parse(s: &str) -> Result<Timestamp> {
        parse_canonical(s).map(Timestamp).map_err(|why| {
            Error::invalid(format!("invalid timestamp {s:?}: {why} (expected YYYY-MM-DDThh:mm:ss.fffZ)"))
        })
    }

    /// Lenient RFC 3339 (any fractional precision, any offset) → UTC ms.
    /// For user-supplied filters like `recordedAfter`.
    pub fn parse_rfc3339(s: &str) -> Result<Timestamp> {
        parse_rfc3339(s)
            .map(Timestamp)
            .map_err(|why| Error::invalid(format!("invalid RFC 3339 timestamp {s:?}: {why}")))
    }

    pub fn format(&self) -> String {
        let (days, ms_of_day) = (self.0.div_euclid(MS_PER_DAY), self.0.rem_euclid(MS_PER_DAY));
        let (y, m, d) = civil_from_days(days);
        let time = format!(
            "{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            ms_of_day / MS_PER_HOUR,
            ms_of_day / MS_PER_MIN % 60,
            ms_of_day / MS_PER_SEC % 60,
            ms_of_day % MS_PER_SEC
        );
        if (0..=9999).contains(&y) {
            format!("{y:04}-{time}")
        } else {
            // Reachable for valid data: EDTF `9999` ends at 10000-01-01, and an offset can
            // push year 0000 into −0001. ISO 8601's expanded year keeps the output exact;
            // `parse` rejects it, so it can never become a claim's `at`.
            format!("{y:+05}-{time}")
        }
    }

    /// Whether `format` yields the canonical 4-digit-year form that `parse` accepts.
    pub(crate) fn in_canonical_range(&self) -> bool {
        (days_from_civil(0, 1, 1) * MS_PER_DAY..days_from_civil(10000, 1, 1) * MS_PER_DAY).contains(&self.0)
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
    ///
    /// For intervals, qualifiers on either side set the flags.
    pub fn parse(s: &str) -> Result<When> {
        parse_when(s).map_err(|why| Error::invalid(format!("invalid EDTF {s:?}: {why}")))
    }

    /// Midpoint used for ordering/distance; `None` if either end is open.
    pub fn center(&self) -> Option<i64> {
        let (lo, hi) = (self.lo?, self.hi?);
        // i128 because the fields are public and lo + hi may overflow i64; the floored
        // midpoint lies between lo and hi, so narrowing it back is lossless.
        Some((i128::from(lo) + i128::from(hi)).div_euclid(2) as i64)
    }

    /// The fields are public; a `When` built in code must still be what `raw` parses
    /// to, because only `raw` is serialized.
    pub(crate) fn check_consistent(&self) -> Result<()> {
        if When::parse(&self.raw)? != *self {
            return Err(Error::invalid(format!("EDTF {:?}: range/flags do not match its text", self.raw)));
        }
        Ok(())
    }
}

/// ISO 8601 duration subset for `window`: `PnD`, `PTnH`, `PTnM`, `PnDTnH…`, `PnW`.
/// Years/months are rejected (ambiguous length) → `invalid`.
pub fn parse_duration_ms(s: &str) -> Result<i64> {
    parse_duration(s).map_err(|why| {
        Error::invalid(format!("invalid duration {s:?}: {why} (expected e.g. P1D, PT6H, P1DT12H30M, P2W)"))
    })
}

// ---- civil calendar (proleptic Gregorian), after H. Hinnant's `days_from_civil` ----

/// Days since 1970-01-01 of a valid date.
const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        2 if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// ---- lexing ----

struct Cursor<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor { s: s.as_bytes(), pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        let hit = self.peek() == Some(b);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn literal(&mut self, b: u8) -> std::result::Result<(), String> {
        if self.eat(b) { Ok(()) } else { Err(format!("expected '{}' at position {}", b as char, self.pos)) }
    }

    /// Exactly `n` ASCII digits.
    fn digits(&mut self, n: usize) -> std::result::Result<u32, String> {
        let end = self.pos + n;
        let ds = self
            .s
            .get(self.pos..end)
            .filter(|ds| ds.iter().all(u8::is_ascii_digit))
            .ok_or_else(|| format!("expected {n} digits at position {}", self.pos))?;
        self.pos = end;
        Ok(ds.iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0')))
    }

    /// The longest run of ASCII digits (possibly empty).
    fn digit_run(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        &self.s[start..self.pos]
    }

    fn at_end(&self) -> bool {
        self.pos == self.s.len()
    }

    fn end(&self) -> std::result::Result<(), String> {
        if self.at_end() {
            Ok(())
        } else {
            Err(format!("unexpected {:?} at position {}", String::from_utf8_lossy(&self.s[self.pos..]), self.pos))
        }
    }
}

fn day_number(y: u32, m: u32, d: u32) -> std::result::Result<i64, String> {
    if !(1..=12).contains(&m) {
        return Err(format!("month {m:02} out of range"));
    }
    if d == 0 || d > days_in_month(y, m) {
        return Err(format!("day {d:02} out of range for {y:04}-{m:02}"));
    }
    Ok(days_from_civil(i64::from(y), m, d))
}

/// `YYYY-MM-DD` → days since the epoch.
fn date(c: &mut Cursor) -> std::result::Result<i64, String> {
    let y = c.digits(4)?;
    c.literal(b'-')?;
    let m = c.digits(2)?;
    c.literal(b'-')?;
    let d = c.digits(2)?;
    day_number(y, m, d)
}

fn clock_ms(h: u32, min: u32, sec: u32) -> std::result::Result<i64, String> {
    if h > 23 {
        return Err(format!("hour {h:02} out of range"));
    }
    if min > 59 {
        return Err(format!("minute {min:02} out of range"));
    }
    if sec > 59 {
        return Err(format!("second {sec:02} out of range (leap seconds are not representable)"));
    }
    Ok(i64::from(h) * MS_PER_HOUR + i64::from(min) * MS_PER_MIN + i64::from(sec) * MS_PER_SEC)
}

/// Milliseconds of a fraction's digits (truncated past 3 digits: floor, since the
/// fraction is a positive addend).
fn fraction_ms(digits: &[u8]) -> i64 {
    digits.iter().chain([b'0'; 3].iter()).take(3).fold(0, |acc, d| acc * 10 + i64::from(d - b'0'))
}

/// `±hh:mm` → signed ms east of UTC, or `None` if no sign follows.
fn numeric_offset(c: &mut Cursor) -> std::result::Result<Option<i64>, String> {
    let sign = if c.eat(b'+') {
        1
    } else if c.eat(b'-') {
        -1
    } else {
        return Ok(None);
    };
    let h = c.digits(2)?;
    c.literal(b':')?;
    let m = c.digits(2)?;
    if h > 23 || m > 59 {
        return Err(format!("offset {h:02}:{m:02} out of range"));
    }
    Ok(Some(sign * (i64::from(h) * MS_PER_HOUR + i64::from(m) * MS_PER_MIN)))
}

fn parse_canonical(s: &str) -> std::result::Result<i64, String> {
    let mut c = Cursor::new(s);
    let days = date(&mut c)?;
    c.literal(b'T')?;
    let h = c.digits(2)?;
    c.literal(b':')?;
    let min = c.digits(2)?;
    c.literal(b':')?;
    let sec = c.digits(2)?;
    c.literal(b'.')?;
    let ms = c.digits(3)?;
    c.literal(b'Z')?;
    c.end()?;
    Ok(days * MS_PER_DAY + clock_ms(h, min, sec)? + i64::from(ms))
}

/// RFC 3339 `date-time`; `T` and `Z` may be lowercase (RFC 3339 §5.6).
fn parse_rfc3339(s: &str) -> std::result::Result<i64, String> {
    let mut c = Cursor::new(s);
    let days = date(&mut c)?;
    if !(c.eat(b'T') || c.eat(b't')) {
        return Err(format!("expected 'T' at position {}", c.pos));
    }
    let h = c.digits(2)?;
    c.literal(b':')?;
    let min = c.digits(2)?;
    c.literal(b':')?;
    let sec = c.digits(2)?;
    let frac = if c.eat(b'.') {
        let digits = c.digit_run();
        if digits.is_empty() {
            return Err(format!("expected fraction digits at position {}", c.pos));
        }
        fraction_ms(digits)
    } else {
        0
    };
    let offset = if c.eat(b'Z') || c.eat(b'z') {
        0
    } else {
        numeric_offset(&mut c)?.ok_or_else(|| format!("expected 'Z' or ±hh:mm at position {}", c.pos))?
    };
    c.end()?;
    Ok(days * MS_PER_DAY + clock_ms(h, min, sec)? + frac - offset)
}

// ---- EDTF ----

/// One side of an interval, or a whole non-interval value.
struct Point {
    lo: i64,
    hi: i64,
    uncertain: bool,
    approximate: bool,
    local_unknown: bool,
}

fn parse_when(s: &str) -> std::result::Result<When, String> {
    let (lo, hi, points) = match s.split_once('/') {
        None => {
            let p = point(s)?;
            (Some(p.lo), Some(p.hi), vec![p])
        }
        Some((_, end)) if end.contains('/') => return Err("an interval has exactly one '/'".into()),
        Some(("..", "..")) => return Err("an interval needs at least one closed end".into()),
        Some(("..", end)) => {
            let end = point(end)?;
            (None, Some(end.hi), vec![end])
        }
        Some((start, "..")) => {
            let start = point(start)?;
            (Some(start.lo), None, vec![start])
        }
        Some((start, end)) => {
            let (start, end) = (point(start)?, point(end)?);
            if start.lo > end.lo {
                return Err("interval end is before its start".into());
            }
            (Some(start.lo), Some(end.hi), vec![start, end])
        }
    };
    Ok(When {
        raw: s.to_string(),
        lo,
        hi,
        uncertain: points.iter().any(|p| p.uncertain),
        approximate: points.iter().any(|p| p.approximate),
        local_unknown: points.iter().any(|p| p.local_unknown),
    })
}

fn point(s: &str) -> std::result::Result<Point, String> {
    let (body, uncertain, approximate) = match s.as_bytes().last() {
        Some(b'?') => (&s[..s.len() - 1], true, false),
        Some(b'~') => (&s[..s.len() - 1], false, true),
        Some(b'%') => (&s[..s.len() - 1], true, true),
        _ => (s, false, false),
    };
    let qualified = uncertain || approximate;
    let date_range = |lo_days: i64, hi_days: i64| Point {
        lo: lo_days * MS_PER_DAY,
        hi: hi_days * MS_PER_DAY,
        uncertain,
        approximate,
        local_unknown: false,
    };

    if body.len() == 4 && body.ends_with('X') {
        let known = body.trim_end_matches('X');
        if !(2..=3).contains(&known.len()) || !known.bytes().all(|b| b.is_ascii_digit()) {
            return Err("an unspecified year has one or two trailing X digits, like 199X or 19XX".into());
        }
        if qualified {
            return Err("qualifiers apply to YYYY, YYYY-MM and YYYY-MM-DD only".into());
        }
        let span = 10_i64.pow(4 - known.len() as u32);
        let first: i64 = known.bytes().fold(0, |acc, d| acc * 10 + i64::from(d - b'0')) * span;
        return Ok(date_range(days_from_civil(first, 1, 1), days_from_civil(first + span, 1, 1)));
    }

    let mut c = Cursor::new(body);
    let y = c.digits(4)?;
    if c.at_end() {
        let y = i64::from(y);
        return Ok(date_range(days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)));
    }
    c.literal(b'-')?;
    let m = c.digits(2)?;
    if c.at_end() {
        let first = day_number(y, m, 1)?;
        return Ok(date_range(first, first + i64::from(days_in_month(y, m))));
    }
    c.literal(b'-')?;
    let d = c.digits(2)?;
    let day = day_number(y, m, d)?;
    if c.at_end() {
        return Ok(date_range(day, day + 1));
    }

    // datetime: YYYY-MM-DDThh:mm[:ss[.fff]] then Z, ±hh:mm, or nothing (zone unknown)
    if qualified {
        return Err("qualifiers apply to dates, not datetimes".into());
    }
    c.literal(b'T')?;
    let h = c.digits(2)?;
    c.literal(b':')?;
    let min = c.digits(2)?;
    let (sec, frac) = if c.eat(b':') {
        let sec = c.digits(2)?;
        let frac = if c.eat(b'.') {
            let digits = c.digit_run();
            if !(1..=3).contains(&digits.len()) {
                return Err("fractional seconds take 1 to 3 digits".into());
            }
            fraction_ms(digits)
        } else {
            0
        };
        (sec, frac)
    } else {
        (0, 0)
    };
    let offset = if c.eat(b'Z') { Some(0) } else { numeric_offset(&mut c)? };
    c.end()?;
    let local = day * MS_PER_DAY + clock_ms(h, min, sec)? + frac;
    Ok(match offset {
        Some(offset) => {
            let utc = local - offset;
            Point { lo: utc, hi: utc + 1, uncertain, approximate, local_unknown: false }
        }
        None => Point {
            lo: local - LOCAL_UNKNOWN_MS,
            hi: local + LOCAL_UNKNOWN_MS + 1,
            uncertain,
            approximate,
            local_unknown: true,
        },
    })
}

// ---- durations ----

fn parse_duration(s: &str) -> std::result::Result<i64, String> {
    let body = s.strip_prefix('P').ok_or("must start with 'P'")?;
    let (date_part, time_part) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };
    if date_part.contains(['Y', 'M']) {
        return Err("years and months have no fixed length; use weeks or days".into());
    }
    let (date_ms, date_units) = components(date_part, &[(b'W', 7 * MS_PER_DAY), (b'D', MS_PER_DAY)])?;
    let (time_ms, time_units) = match time_part {
        Some(t) => {
            let parsed = components(t, &[(b'H', MS_PER_HOUR), (b'M', MS_PER_MIN), (b'S', MS_PER_SEC)])?;
            if parsed.1 == 0 {
                return Err("'T' must be followed by hours, minutes or seconds".into());
            }
            parsed
        }
        None => (0, 0),
    };
    if date_part.contains('W') && date_units + time_units > 1 {
        return Err("PnW cannot be combined with other units".into());
    }
    if date_units + time_units == 0 {
        return Err("no components".into());
    }
    date_ms.checked_add(time_ms).ok_or_else(|| "too large".into())
}

/// `(digits unit)*` with each unit at most once, in the order of `units`.
/// Returns the total in ms and the number of components.
fn components(part: &str, units: &[(u8, i64)]) -> std::result::Result<(i64, usize), String> {
    let mut c = Cursor::new(part);
    let (mut total, mut count, mut next) = (0_i64, 0_usize, 0_usize);
    while !c.at_end() {
        let digits = c.digit_run();
        if digits.is_empty() {
            return Err(format!("expected a whole number at position {}", c.pos));
        }
        let unit = c.peek().ok_or("number without a unit")?;
        let idx = units[next..]
            .iter()
            .position(|(u, _)| *u == unit)
            .ok_or_else(|| format!("unexpected '{}' (units must be integers, each once, in order)", unit as char))?;
        let (_, unit_ms) = units[next + idx];
        c.pos += 1;
        let n: i64 = std::str::from_utf8(digits)
            .map_err(|e| e.to_string())?
            .parse()
            .map_err(|_| "number too large".to_string())?;
        total = n.checked_mul(unit_ms).and_then(|ms| total.checked_add(ms)).ok_or("too large")?;
        next += idx + 1;
        count += 1;
    }
    Ok((total, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_every_day_of_years_0000_to_9999() {
        let (first, last) = (days_from_civil(0, 1, 1), days_from_civil(9999, 12, 31));
        let mut expected = (0_i64, 1_u32, 1_u32);
        for day in first..=last {
            let civil = civil_from_days(day);
            assert_eq!(civil, expected, "day {day}");
            assert_eq!(days_from_civil(civil.0, civil.1, civil.2), day);
            // advance the expected date by one calendar day
            let (y, m, d) = expected;
            expected = if d < days_in_month(y as u32, m) {
                (y, m, d + 1)
            } else if m < 12 {
                (y, m + 1, 1)
            } else {
                (y + 1, 1, 1)
            };
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}
