//! Per-actor append-only logs (DESIGN §5.1): `log/<actor>/<first-seq>.jsonl`, written
//! once against the `Logs` store.
//!
//! Layout invariant: every segment holds a contiguous run of seqs and is named by
//! its first seq; no seq appears twice. Local appends hand one batch to the store
//! (a directory extends the newest segment, rotating at `SEGMENT_LINES`); merges write
//! each contiguous run of new claims as new segments, so a partial replica may have
//! gaps that a later merge fills with a segment whose name sorts between existing
//! ones. Because the segments of one actor are disjoint contiguous ranges, ordering
//! them by first seq and reading them in turn yields ascending seq. Readers check that
//! order as they go, so a broken invariant surfaces as an `integrity` error, never as
//! out-of-order output.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

use knowa_model::{Claim, Error, ErrorCode, Id, IdKind, Result};
use serde_json::json;

use crate::backend::{Guard, Logs, Segment};
use crate::{Head, ImportReport, LOG_DIR, SEGMENT_LINES};

/// `SEGMENT_LINES` as a slice length (10 000 fits every usize).
pub(crate) const SEGMENT_LIMIT: usize = SEGMENT_LINES as usize;

pub(crate) fn segment_name(first: u64) -> String {
    format!("{first:012}.jsonl")
}

pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".jsonl")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let first: u64 = digits.parse().ok()?;
    (first >= 1 && segment_name(first) == name).then_some(first)
}

/// `log/<actor>/<segment>.jsonl` as a relative path (also the bundle entry path).
pub(crate) fn parse_segment_path(rel: &str) -> Option<(Id, u64)> {
    let mut parts = rel.split('/');
    let (Some(LOG_DIR), Some(actor), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let actor = Id::parse(actor).ok().filter(|id| id.kind() == IdKind::Actor)?;
    Some((actor, parse_segment_name(name)?))
}

pub(crate) fn segment_rel_path(actor: &Id, first: u64) -> String {
    format!("{LOG_DIR}/{actor}/{}", segment_name(first))
}

/// Turns scan problems into one `integrity` error (for callers that cannot proceed).
pub(crate) fn fail_on(problems: Vec<String>) -> Result<()> {
    match problems.first() {
        None => Ok(()),
        Some(first) => Err(Error::integrity(first.clone()).with_detail(json!({ "problems": problems }))),
    }
}

fn segments(logs: &dyn Logs, actor: &Id) -> Result<Vec<Segment>> {
    let (segments, problems) = logs.segments(actor)?;
    fail_on(problems)?;
    Ok(segments)
}

/// Parses one log line (including its `\n`). Structural only; no signature check.
pub(crate) fn parse_line(line: &[u8], loc: &dyn Fn() -> String) -> Result<Claim> {
    let body = line
        .strip_suffix(b"\n")
        .ok_or_else(|| Error::integrity(format!("{}: torn line (no trailing newline)", loc())))?;
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Error::integrity(format!("{}: {e}", loc())))?;
    Claim::from_json(&v).map_err(|e| {
        // A claim from a newer format version is not corruption; keep `unsupported`.
        let code = if e.code == ErrorCode::Unsupported { ErrorCode::Unsupported } else { ErrorCode::Integrity };
        Error::new(code, format!("{}: {}", loc(), e.message))
    })
}

/// `Claim::verify` for a claim read from a log. Whatever the model calls the failure,
/// a logged claim that does not verify was not written by an honest append: it is
/// forged or corrupted, so `integrity`.
fn verify_logged(c: &Claim, loc: &dyn Fn() -> String) -> Result<()> {
    c.verify().map_err(|e| Error::integrity(format!("{}: {}", loc(), e.message)))
}

fn ownership_problem(c: &Claim, space: &Id, actor: &Id) -> Option<String> {
    if c.body.actor != *actor {
        return Some(format!("claim {} is by {}, found in the log of {}", c.id, c.body.actor, actor));
    }
    if c.body.space != *space {
        return Some(format!("claim {} belongs to space {}, not {}", c.id, c.body.space, space));
    }
    None
}

/// Well-formed and authentic (`verify` validates the body first), and in `space`.
/// `wrong_space` is the code for the last check: a caller bug for local appends,
/// corrupt input for imports.
pub(crate) fn check_claim(c: &Claim, space: &Id, wrong_space: ErrorCode) -> Result<()> {
    c.verify()?;
    if c.body.space != *space {
        return Err(Error::new(
            wrong_space,
            format!("claim {} belongs to space {}, not {}", c.id, c.body.space, space),
        ));
    }
    Ok(())
}

/// The newest claim of `actor` (highest seq). Read under the actor's exclusive lock,
/// after torn-tail repair, from the last line of the newest segment only. The head is
/// signature-checked because new claims chain onto its id.
pub(crate) fn tail(logs: &dyn Logs, space: &Id, actor: &Id) -> Result<Option<Head>> {
    let Some(segment) = segments(logs, actor)?.pop() else {
        return Ok(None);
    };
    let last = logs.last_line(actor, &segment)?;
    let line = match last.line {
        Some(line) if last.trailing == 0 => line,
        Some(_) => return Err(Error::integrity(format!("{}: torn last line (no trailing newline)", segment.name))),
        None if last.trailing == 0 => return Err(Error::integrity(format!("{}: empty segment", segment.name))),
        None => return Err(Error::integrity(format!("{}: no complete line", segment.name))),
    };
    let claim = parse_line(&line, &|| format!("{}: last line", segment.name))?;
    if let Some(p) = ownership_problem(&claim, space, actor) {
        return Err(Error::integrity(format!("{}: {p}", segment.name)));
    }
    if claim.body.seq < segment.first {
        return Err(Error::integrity(format!(
            "{}: last seq {} is below the segment's first seq {}",
            segment.name, claim.body.seq, segment.first
        )));
    }
    verify_logged(&claim, &|| format!("{}: last line", segment.name))?;
    Ok(Some(Head { seq: claim.body.seq, id: claim.id }))
}

/// Whether `actor`'s log may hold a claim with seq above `seq`, judged without any lock
/// from the last complete line of the newest segment (DESIGN §5.4 catch-up). An
/// unterminated final line is a batch still being written, or one a crash left for the
/// next writer to truncate, and is ignored. `true` whenever it cannot tell, so that the
/// caller reads the log under the actor's lock and gets the authoritative answer.
pub(crate) fn may_have_after(logs: &dyn Logs, actor: &Id, seq: u64) -> Result<bool> {
    let (segments, problems) = logs.segments(actor)?;
    let Some(newest) = segments.last() else {
        // Misnamed entries and nothing else: the locked read reports them.
        return Ok(!problems.is_empty());
    };
    let Some(line) = logs.last_line(actor, newest)?.line else {
        // A newest segment without a complete line, which no append leaves behind: the
        // locked read reports it.
        return Ok(true);
    };
    match parse_line(&line, &|| format!("{}: last line", newest.name)) {
        Ok(claim) => Ok(claim.body.seq > seq),
        // Fallback: an unlocked peek cannot tell a corrupt line from one a shared
        // filesystem has not yet shown in full. The locked read decides, and reports
        // corruption as `integrity`.
        Err(_) => Ok(true),
    }
}

/// Streams one actor's claims with seq ≥ `from_seq`, ascending. Checks structure,
/// ownership and ordering, and verifies (id + signature) every claim it yields: logs
/// are never trusted (DESIGN §5.3). Lines below `from_seq` are only parsed for ordering.
///
/// The actor's lock is held for as long as the iterator lives: exclusively by the
/// caller, or as `guard`, a shared lock the iterator releases when dropped. Under it,
/// an unterminated final line of the newest segment after at least one complete line
/// is an append that never returned, which the next exclusive lock truncates: the
/// iteration ends before it instead of failing.
pub(crate) struct LogIter<'a> {
    logs: &'a dyn Logs,
    space: Id,
    actor: Id,
    from_seq: u64,
    segments: std::vec::IntoIter<Segment>,
    current: Option<Open>,
    last_seq: u64,
    done: bool,
    _guard: Option<Guard>,
}

struct Open {
    name: String,
    reader: Box<dyn BufRead + Send>,
    first: u64,
    next_seq: u64,
    /// the actor's newest segment, the only one an append extends
    newest: bool,
}

impl<'a> LogIter<'a> {
    pub(crate) fn new(logs: &'a dyn Logs, space: &Id, actor: &Id, from_seq: u64, guard: Option<Guard>) -> Result<LogIter<'a>> {
        let all = segments(logs, actor)?;
        // Segment i covers at most [first_i, first_{i+1}); skip those entirely below from_seq.
        let skip = all.windows(2).take_while(|w| w[1].first <= from_seq).count();
        let segments: Vec<Segment> = all.into_iter().skip(skip).collect();
        Ok(LogIter {
            logs,
            space: space.clone(),
            actor: actor.clone(),
            from_seq,
            segments: segments.into_iter(),
            current: None,
            last_seq: 0,
            done: false,
            _guard: guard,
        })
    }

    fn step(&mut self) -> Result<Option<Claim>> {
        loop {
            let mut open = match self.current.take() {
                Some(open) => open,
                None => {
                    let Some(seg) = self.segments.next() else {
                        return Ok(None);
                    };
                    let reader = self.logs.open_segment(&self.actor, &seg)?;
                    let newest = self.segments.len() == 0;
                    Open { name: seg.name, reader, first: seg.first, next_seq: seg.first, newest }
                }
            };
            let mut line = Vec::new();
            let n = open
                .reader
                .read_until(b'\n', &mut line)
                .map_err(|e| Error::internal(format!("io: {}: {e}", open.name)))?;
            if n == 0 {
                if open.next_seq == open.first {
                    return Err(Error::integrity(format!("{}: empty segment", open.name)));
                }
                continue;
            }
            if open.newest && open.next_seq > open.first && !line.ends_with(b"\n") {
                return Ok(None);
            }
            let expected = open.next_seq;
            let claim = parse_line(&line, &|| format!("{}: seq {expected}", open.name))?;
            if let Some(p) = ownership_problem(&claim, &self.space, &self.actor) {
                return Err(Error::integrity(format!("{}: {p}", open.name)));
            }
            if claim.body.seq != expected || claim.body.seq <= self.last_seq {
                return Err(Error::integrity(format!(
                    "{}: found seq {} where seq {expected} was expected (segments must be contiguous, disjoint, and named by their first seq)",
                    open.name, claim.body.seq
                )));
            }
            if claim.body.seq >= self.from_seq {
                verify_logged(&claim, &|| format!("{}: seq {expected}", open.name))?;
            }
            open.next_seq += 1;
            self.current = Some(open);
            self.last_seq = claim.body.seq;
            if claim.body.seq >= self.from_seq {
                return Ok(Some(claim));
            }
        }
    }
}

impl Iterator for LogIter<'_> {
    type Item = Result<Claim>;

    fn next(&mut self) -> Option<Result<Claim>> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(c)) => Some(Ok(c)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// seq → (id, prev) for one actor's existing claims.
pub(crate) type Chain = BTreeMap<u64, (Id, Option<Id>)>;

/// The caller holds `actor`'s exclusive lock.
pub(crate) fn chain(logs: &dyn Logs, space: &Id, actor: &Id) -> Result<Chain> {
    let mut out = Chain::new();
    for claim in LogIter::new(logs, space, actor, 1, None)? {
        let claim = claim?;
        out.insert(claim.body.seq, (claim.id, claim.body.prev));
    }
    Ok(out)
}

/// Missing seq runs `(first, last)` below the highest present seq.
pub(crate) fn gaps(seqs: &BTreeSet<u64>) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut next = 1;
    for &s in seqs {
        if s > next {
            out.push((next, s - 1));
        }
        next = s + 1;
    }
    out
}

/// Appends locally-authored claims of `actor`, whose exclusive lock the caller holds:
/// all are checked against the head before any byte is written, then handed to the
/// store as one batch.
pub(crate) fn append(logs: &dyn Logs, space: &Id, actor: &Id, claims: &[Claim]) -> Result<()> {
    if claims.is_empty() {
        return Ok(());
    }
    let (first_new, mut prev) = match tail(logs, space, actor)? {
        Some(head) => (head.seq + 1, Some(head.id)),
        None => (1, None),
    };
    let mut next_seq = first_new;
    let mut lines = Vec::with_capacity(claims.len());
    for c in claims {
        check_claim(c, space, ErrorCode::Invalid)?;
        if c.body.seq != next_seq || c.body.prev != prev {
            return Err(Error::conflict(format!(
                "claim {} does not continue the chain of {}: expected seq {}, got seq {}",
                c.id, c.body.actor, next_seq, c.body.seq
            ))
            .with_detail(json!({
                "actor": c.body.actor,
                "expectedSeq": next_seq,
                "expectedPrev": prev,
                "seq": c.body.seq,
                "prev": c.body.prev,
            })));
        }
        lines.push(c.to_line()?);
        next_seq += 1;
        prev = Some(c.id.clone());
    }
    logs.append(actor, first_new, &lines)
}

fn equivocation(actor: &Id, seq: u64, have: &Id, got: &Id) -> Error {
    Error::integrity(format!("equivocation: {actor} has two different claims at seq {seq} ({have} and {got})"))
        .with_detail(json!({ "actor": actor, "seq": seq, "ids": [have, got] }))
}

/// What a merge will write; computed (and fully validated) before writing.
pub(crate) struct MergePlan {
    runs: Vec<(Id, u64, Vec<Vec<u8>>)>,
    report: ImportReport,
}

/// Plans merging already-checked `claims` against the chains `existing` returns.
/// Rejects equivocation (same actor+seq, different id) and any adjacency where
/// both sides are known but `prev` does not link them — including an existing
/// successor of a gap-filling claim.
pub(crate) fn plan_merge(claims: &[Claim], mut existing: impl FnMut(&Id) -> Result<Chain>) -> Result<MergePlan> {
    let mut by_actor: BTreeMap<&Id, BTreeMap<u64, &Claim>> = BTreeMap::new();
    for c in claims {
        match by_actor.entry(&c.body.actor).or_default().entry(c.body.seq) {
            Entry::Vacant(v) => {
                v.insert(c);
            }
            // The same claim listed twice is one claim; dropping the repeat loses nothing.
            Entry::Occupied(o) if o.get().id == c.id => {}
            Entry::Occupied(o) => return Err(equivocation(&c.body.actor, c.body.seq, &o.get().id, &c.id)),
        }
    }
    let mut report = ImportReport::default();
    let mut runs = Vec::new();
    for (actor, incoming) in by_actor {
        let have = existing(actor)?;
        let mut new: BTreeMap<u64, &Claim> = BTreeMap::new();
        for (seq, c) in incoming {
            match have.get(&seq) {
                Some((id, _)) if *id == c.id => report.skipped_existing += 1,
                Some((id, _)) => return Err(equivocation(actor, seq, id, &c.id)),
                None => {
                    new.insert(seq, c);
                }
            }
        }
        for (&seq, c) in &new {
            // seq ≥ 1 is guaranteed by `check_claim`; seq 1 has nothing below it.
            let below_seq = seq.saturating_sub(1);
            let below = new.get(&below_seq).map(|b| &b.id).or_else(|| have.get(&below_seq).map(|(id, _)| id));
            if let Some(below) = below
                && c.body.prev.as_ref() != Some(below)
            {
                return Err(Error::integrity(format!(
                    "broken chain: claim {} (seq {seq} of {actor}) has prev {:?}, but seq {} is {below}",
                    c.id,
                    c.body.prev.as_ref().map(Id::as_str),
                    below_seq
                )));
            }
            if let Some((above, above_prev)) = have.get(&(seq + 1))
                && above_prev.as_ref() != Some(&c.id)
            {
                return Err(Error::integrity(format!(
                    "broken chain: existing claim {above} (seq {} of {actor}) does not link to incoming {} at seq {seq}",
                    seq + 1,
                    c.id
                )));
            }
        }
        report.added += new.len() as u64;
        for (first, lines) in line_runs(new.iter().map(|(&seq, &c)| (seq, c)))? {
            runs.push((actor.clone(), first, lines));
        }
        let mut seqs: BTreeSet<u64> = have.keys().copied().collect();
        seqs.extend(new.keys().copied());
        report.gaps.extend(gaps(&seqs).into_iter().map(|(lo, hi)| (actor.clone(), lo, hi)));
    }
    Ok(MergePlan { runs, report })
}

impl MergePlan {
    /// True when `apply` will write at least one segment.
    pub(crate) fn writes_segments(&self) -> bool {
        !self.runs.is_empty()
    }

    /// Writes every planned run as new segments. The caller holds every planned actor's
    /// exclusive lock and has already written a fresh rebuild generation: index catch-up
    /// cannot see a run that fills a gap below an indexed head (DESIGN §5.4).
    pub(crate) fn apply(self, logs: &dyn Logs) -> Result<ImportReport> {
        for (actor, first, lines) in &self.runs {
            logs.write_run(actor, *first, lines)?;
        }
        Ok(self.report)
    }
}

/// Groups ascending `(seq, claim)` pairs into contiguous runs of log lines,
/// each keyed by its first seq.
pub(crate) fn line_runs<'a>(claims: impl IntoIterator<Item = (u64, &'a Claim)>) -> Result<Vec<(u64, Vec<Vec<u8>>)>> {
    let mut runs: Vec<(u64, Vec<Vec<u8>>)> = Vec::new();
    for (seq, c) in claims {
        let line = c.to_line()?;
        match runs.last_mut() {
            Some((first, lines)) if *first + lines.len() as u64 == seq => lines.push(line),
            _ => runs.push((seq, vec![line])),
        }
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_are_canonical() {
        assert_eq!(segment_name(42), "000000000042.jsonl");
        assert_eq!(parse_segment_name("000000000042.jsonl"), Some(42));
        for bad in ["42.jsonl", "000000000000.jsonl", "00000000042a.jsonl", "000000000042.json"] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
        assert_eq!(gaps(&[3, 4, 7].into_iter().collect()), [(1, 2), (5, 6)]);
    }
}
