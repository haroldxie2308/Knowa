//! The feed primitive (DESIGN §6 feed): claims in local arrival order, from a cursor
//! that stays valid across a reindex and on every machine that reads the replica.
//!
//! **Cursor format, version 1.** Opaque to clients, but deterministic and validated:
//!
//! ```text
//! fc1.<gaps>(.<actor>.<seq>)*
//! ```
//!
//! - `<gaps>` is the replica's gap generation (DESIGN §5.4), read before the index
//!   serving the page is opened, or empty when the replica has none.
//! - Each `<actor>.<seq>` pair is an `a_` actor id and the highest seq of that actor
//!   already delivered, in decimal without leading zeros, within 1..=2^53−1. Pairs
//!   are sorted by actor id, one per actor. An actor without a pair has had nothing
//!   delivered.
//!
//! No field can contain `.` and every character is URL- and shell-safe, so the fields
//! need no further encoding (actor ids are base32 already). A cursor is accepted only
//! if encoding what it parses to gives back the same text, so each state has exactly
//! one cursor. Anything else → `invalid`.
//!
//! Per-actor seqs mean the same on every index of a replica, so a cursor holds as long
//! as the replica's gap generation does: across a reindex, and across merges that only
//! add claims above every actor's head (which land above every seq a cursor has
//! passed) or add blobs, which rebuild indexes without changing any seq. A new gap
//! generation means a merge filled gaps in the logs, possibly below seqs the cursor has
//! passed → `cursor_expired`.

use std::collections::BTreeMap;

use knowa_model::query::{FeedResult, MAX_LIMIT};
use knowa_model::value::MAX_SAFE_INT;
use knowa_model::{Error, Id, IdKind, Result};

use crate::select::Space;

/// Version tag of the cursor format in the module docs.
const CURSOR_VERSION: &str = "fc1";
/// Separates the cursor's fields.
const SEP: char = '.';

/// A feed position: what has been delivered, valid at one gap generation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// the replica's gap generation
    gaps: Option<String>,
    /// the highest seq delivered, per actor
    delivered: BTreeMap<Id, u64>,
}

impl Cursor {
    /// Nothing delivered yet, at gap generation `gaps`, which comes from the replica.
    /// One that the format cannot carry → `internal`: the store's rule for the `gaps`
    /// file and this format disagree.
    fn start(gaps: Option<&str>) -> Result<Cursor> {
        if let Some(g) = gaps.filter(|g| !is_ulid(g)) {
            return Err(Error::internal(format!("gap generation {g:?} cannot be recorded in a feed cursor")));
        }
        Ok(Cursor { gaps: gaps.map(str::to_string), delivered: BTreeMap::new() })
    }

    /// Accepts exactly the texts `encode` produces; anything else → `invalid`.
    pub(crate) fn parse(s: &str) -> Result<Cursor> {
        let malformed = |why: String| {
            Error::invalid(format!("malformed feed cursor {s:?}: {why}; pass a cursor back exactly as feed returned it"))
        };
        let rest = s
            .strip_prefix(CURSOR_VERSION)
            .and_then(|r| r.strip_prefix(SEP))
            .ok_or_else(|| malformed(format!("it does not start with {CURSOR_VERSION}{SEP}")))?;
        let mut fields = rest.split(SEP);
        let gaps = match fields.next() {
            // `split` always yields a first field; empty means the replica had no gap
            // generation.
            None | Some("") => None,
            Some(g) if is_ulid(g) => Some(g.to_string()),
            Some(g) => return Err(malformed(format!("{g:?} is not a gap generation"))),
        };
        let mut delivered = BTreeMap::new();
        while let Some(actor) = fields.next() {
            let actor = Id::parse(actor)
                .ok()
                .filter(|a| a.kind() == IdKind::Actor)
                .ok_or_else(|| malformed(format!("{actor:?} is not an actor id")))?;
            let seq = fields.next().ok_or_else(|| malformed(format!("no seq after {actor}")))?;
            let seq = parse_seq(seq).ok_or_else(|| malformed(format!("{seq:?} is not a seq in 1..=2^53−1")))?;
            delivered.insert(actor, seq);
        }
        let cursor = Cursor { gaps, delivered };
        if cursor.encode() != s {
            return Err(malformed("it is not in canonical form (actors sorted, each once; seqs without leading zeros)".into()));
        }
        Ok(cursor)
    }

    fn encode(&self) -> String {
        let mut out = format!("{CURSOR_VERSION}{SEP}");
        // An absent gap generation is the empty field.
        if let Some(g) = &self.gaps {
            out.push_str(g);
        }
        for (actor, seq) in &self.delivered {
            out.push(SEP);
            out.push_str(actor.as_str());
            out.push(SEP);
            out.push_str(&seq.to_string());
        }
        out
    }

    /// Records `seq` of `actor` as delivered.
    fn deliver(&mut self, actor: &Id, seq: u64) {
        self.delivered.entry(actor.clone()).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
    }
}

/// What a replica's `gaps` file holds: one ULID, 26 lowercase base32 characters
/// (knowa-store accepts exactly these). Checked so that a cut-off cursor is `invalid`,
/// not `cursor_expired`.
fn is_ulid(g: &str) -> bool {
    g.len() == 26 && g.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// A claim seq (1..=2^53−1, DESIGN §4.4) in plain decimal; leading zeros are caught by
/// the canonical-form check.
fn parse_seq(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|n| (1..=MAX_SAFE_INT.unsigned_abs()).contains(n))
}

/// DESIGN §6 feed: the claims `cursor` has not delivered (all of them without one), in
/// this machine's arrival order, and the cursor that also covers them. A cursor issued
/// at another gap generation than the replica's now → `cursor_expired`.
pub(crate) fn feed(space: &Space, cursor: Option<&str>, limit: u32) -> Result<FeedResult> {
    check_limit(limit, "limit")?;
    let given = cursor.map(Cursor::parse).transpose()?;
    // Read before the index is opened. A merge that fills gaps writes a fresh `gaps`
    // after its last write, so if this page misses claims of one that lands from here
    // on, the cursor it returns expires on the next call. And one that finished before
    // this read wrote its rebuild generation before its last `gaps`, so the index
    // opened below rebuilds over the gaps it filled.
    let gaps = space.replica.gaps()?;
    let mut next = match given {
        None => Cursor::start(gaps.as_deref())?,
        Some(c) if c.gaps == gaps => c,
        Some(c) => {
            let name = |g: Option<&str>| g.map_or_else(|| "none".to_string(), str::to_string);
            return Err(Error::cursor_expired(format!(
                "feed cursor is from gap generation {}, but the replica of space {} is now at gap generation {}: a merge has filled gaps in its logs since, possibly below seqs this cursor has passed; restart the feed from the beginning, without a cursor (writes are idempotent)",
                name(c.gaps.as_deref()),
                space.name(),
                name(gaps.as_deref())
            ))
            .with_detail(serde_json::json!({ "cursor": cursor, "gaps": gaps })));
        }
    };
    let index = space.index()?;
    let claims = index.feed(&next.delivered, limit)?;
    for c in &claims {
        next.deliver(&c.claim.body.actor, c.claim.body.seq);
    }
    Ok(FeedResult { claims, cursor: next.encode() })
}

/// `1..=MAX_LIMIT`.
pub(crate) fn check_limit(limit: u32, field: &str) -> Result<()> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(Error::invalid(format!("{field} must be between 1 and {MAX_LIMIT}, got {limit}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::{ErrorCode, Signer};

    const GAPS: &str = "01jd5k8v0m3q7x9yz2c4b6n8pr";

    fn actor(n: u8) -> Id {
        Signer::from_seed([n; 32]).actor_id()
    }

    fn cursor(gaps: Option<&str>, delivered: &[(&Id, u64)]) -> Cursor {
        Cursor { gaps: gaps.map(str::to_string), delivered: delivered.iter().map(|(a, s)| ((*a).clone(), *s)).collect() }
    }

    #[test]
    fn cursors_round_trip_in_one_canonical_form() {
        let (a, b) = (actor(1), actor(2));
        let (lo, hi) = if a < b { (&a, &b) } else { (&b, &a) };
        let c = cursor(Some(GAPS), &[(hi, 7), (lo, 42)]);
        let text = c.encode();
        assert_eq!(text, format!("fc1.{GAPS}.{lo}.42.{hi}.7"), "actors sorted by id");
        assert_eq!(Cursor::parse(&text).unwrap(), c);

        let none = cursor(None, &[(&a, 1)]);
        assert_eq!(none.encode(), format!("fc1..{a}.1"), "no gap generation: an empty field");
        assert_eq!(Cursor::parse(&none.encode()).unwrap(), none);

        let start = Cursor::start(None).unwrap();
        assert_eq!(start.encode(), "fc1.");
        assert_eq!(Cursor::parse("fc1.").unwrap(), start);
        assert_eq!(Cursor::parse(&format!("fc1.{GAPS}")).unwrap(), Cursor::start(Some(GAPS)).unwrap());
    }

    #[test]
    fn delivering_keeps_the_highest_seq_per_actor() {
        let (a, b) = (actor(1), actor(2));
        let mut c = cursor(Some(GAPS), &[(&a, 5)]);
        c.deliver(&a, 6);
        c.deliver(&b, 1);
        c.deliver(&a, 3);
        assert_eq!(c, cursor(Some(GAPS), &[(&a, 6), (&b, 1)]));
    }

    #[test]
    fn malformed_cursors_are_invalid() {
        let (a, b) = (actor(1), actor(2));
        let (lo, hi) = if a < b { (&a, &b) } else { (&b, &a) };
        let entity = Id::new_entity().unwrap();
        let max = MAX_SAFE_INT;
        let bad = [
            // the retired `<epoch>:<n>` form
            "7f3a9c0e1d2b4a5f8e9d0c1b2a3f4e5d:42".to_string(),
            String::new(),
            "fc1".into(),
            "fc2.".into(),
            "FC1.".into(),
            " fc1.".into(),
            format!("fc1.{GAPS}."),
            format!("fc1.{}", &GAPS[..20]),
            format!("fc1.{}", GAPS.to_uppercase()),
            format!("fc1.{GAPS}x"),
            format!("fc1.{GAPS}.{a}"),
            format!("fc1.{GAPS}.{a}."),
            format!("fc1.{GAPS}.{a}.0"),
            format!("fc1.{GAPS}.{a}.007"),
            format!("fc1.{GAPS}.{a}.+7"),
            format!("fc1.{GAPS}.{a}.-7"),
            format!("fc1.{GAPS}.{a}.7 "),
            format!("fc1.{GAPS}.{a}.{}", max + 1),
            format!("fc1.{GAPS}.{a}.99999999999999999999999"),
            format!("fc1.{GAPS}.{hi}.1.{lo}.1"),
            format!("fc1.{GAPS}.{a}.1.{a}.2"),
            format!("fc1.{GAPS}.{entity}.1"),
            format!("fc1.{GAPS}.{}.1", &a.as_str()[..20]),
            format!("fc1.{GAPS}.{a}.1:2"),
        ];
        for s in &bad {
            let e = Cursor::parse(s).unwrap_err();
            assert_eq!(e.code, ErrorCode::Invalid, "{s:?}");
            assert!(e.message.contains("pass a cursor back exactly"), "{s:?}: {}", e.message);
        }
        assert!(Cursor::parse(&format!("fc1.{GAPS}.{a}.{max}")).is_ok(), "the largest seq");
    }

    #[test]
    fn a_gap_generation_the_format_cannot_carry_is_internal() {
        assert_eq!(Cursor::start(Some("not.a.ulid")).unwrap_err().code, ErrorCode::Internal);
    }

    #[test]
    fn limit_bounds() {
        assert!(check_limit(1, "limit").is_ok());
        assert!(check_limit(MAX_LIMIT, "limit").is_ok());
        assert_eq!(check_limit(0, "limit").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(check_limit(MAX_LIMIT + 1, "limit").unwrap_err().code, ErrorCode::Invalid);
    }
}
