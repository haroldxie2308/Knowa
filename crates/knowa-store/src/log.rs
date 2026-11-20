//! Per-actor append-only logs (DESIGN §5): `log/<actor>/<first-seq>.jsonl`.
//!
//! Layout invariant: every segment holds a contiguous run of seqs and is named by
//! its first seq; no seq appears twice. Local appends extend the newest segment
//! (rotating at `SEGMENT_LINES`); merges write each contiguous run of new claims as
//! a new segment, so a partial replica may have gaps that a later merge fills with
//! a segment whose name sorts between existing ones. Because the segments of one
//! actor are disjoint contiguous ranges, ordering them by first seq and reading
//! them in turn yields ascending seq. Readers check that order as they go, so a
//! broken invariant surfaces as an `integrity` error, never as out-of-order output.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use knowa_model::{Claim, Error, ErrorCode, Id, IdKind, Result};
use serde_json::json;

use crate::fsutil::{self, TempFile, io_err};
use crate::{Head, ImportReport, LOG_DIR, SEGMENT_LINES};

pub(crate) struct Segment {
    pub(crate) first: u64,
    pub(crate) path: PathBuf,
}

/// `SEGMENT_LINES` as a slice length (10 000 fits every usize).
pub(crate) const SEGMENT_LIMIT: usize = SEGMENT_LINES as usize;

pub(crate) fn segment_name(first: u64) -> String {
    format!("{first:012}.jsonl")
}

fn parse_segment_name(name: &str) -> Option<u64> {
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

fn actor_dir(root: &Path, actor: &Id) -> PathBuf {
    root.join(LOG_DIR).join(actor.as_str())
}

/// Turns scan problems into one `integrity` error (for callers that cannot proceed).
pub(crate) fn fail_on(problems: Vec<String>) -> Result<()> {
    match problems.first() {
        None => Ok(()),
        Some(first) => Err(Error::integrity(first.clone()).with_detail(json!({ "problems": problems }))),
    }
}

/// Actor directories under `log/`, plus anything there that does not belong.
pub(crate) fn scan_actors(root: &Path) -> Result<(Vec<Id>, Vec<String>)> {
    let dir = root.join(LOG_DIR);
    let (mut actors, mut problems) = (Vec::new(), Vec::new());
    // `log/` absent: tools such as git drop empty directories, and no directory
    // means no claims — the same thing an empty one means.
    let Some(entries) = fsutil::read_dir_if_exists(&dir)? else {
        return Ok((actors, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = match fsutil::entry_name(&entry) {
            Ok(n) => n,
            Err(p) => {
                problems.push(p);
                continue;
            }
        };
        if fsutil::is_hidden(&name) {
            continue;
        }
        match Id::parse(&name) {
            Ok(id) if id.kind() == IdKind::Actor && entry.path().is_dir() => actors.push(id),
            _ => problems.push(format!("{}: not an actor log directory", entry.path().display())),
        }
    }
    actors.sort();
    Ok((actors, problems))
}

/// Segments of `actor`, ascending by first seq, plus misnamed entries.
pub(crate) fn scan_segments(root: &Path, actor: &Id) -> Result<(Vec<Segment>, Vec<String>)> {
    let dir = actor_dir(root, actor);
    let (mut segments, mut problems) = (Vec::new(), Vec::new());
    let Some(entries) = fsutil::read_dir_if_exists(&dir)? else {
        return Ok((segments, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = match fsutil::entry_name(&entry) {
            Ok(n) => n,
            Err(p) => {
                problems.push(p);
                continue;
            }
        };
        if fsutil::is_hidden(&name) {
            continue;
        }
        match parse_segment_name(&name) {
            Some(first) => segments.push(Segment { first, path: entry.path() }),
            None => problems.push(format!("{}: not a segment file (<12-digit first seq>.jsonl)", entry.path().display())),
        }
    }
    segments.sort_by_key(|s| s.first);
    Ok((segments, problems))
}

fn segments(root: &Path, actor: &Id) -> Result<Vec<Segment>> {
    let (segments, problems) = scan_segments(root, actor)?;
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

/// The newest claim of an actor and the segment holding it.
pub(crate) struct Tail {
    pub(crate) head: Head,
    pub(crate) segment: Segment,
    /// lines in `segment` (contiguity: last seq − first seq + 1)
    pub(crate) lines: u64,
}

/// Reads only the last line of the newest segment. The head is signature-checked
/// because new claims chain onto its id.
pub(crate) fn tail(root: &Path, space: &Id, actor: &Id) -> Result<Option<Tail>> {
    let Some(segment) = segments(root, actor)?.pop() else {
        return Ok(None);
    };
    let line = read_last_line(&segment.path)?;
    let claim = parse_line(&line, &|| format!("{}: last line", segment.path.display()))?;
    if let Some(p) = ownership_problem(&claim, space, actor) {
        return Err(Error::integrity(format!("{}: {p}", segment.path.display())));
    }
    if claim.body.seq < segment.first {
        return Err(Error::integrity(format!(
            "{}: last seq {} is below the segment's first seq {}",
            segment.path.display(),
            claim.body.seq,
            segment.first
        )));
    }
    claim.verify()?;
    let lines = claim.body.seq - segment.first + 1;
    Ok(Some(Tail { head: Head { seq: claim.body.seq, id: claim.id }, segment, lines }))
}

fn read_last_line(path: &Path) -> Result<Vec<u8>> {
    const CHUNK: u64 = 8 * 1024;
    let mut f = File::open(path).map_err(|e| io_err(path, e))?;
    let len = f.metadata().map_err(|e| io_err(path, e))?.len();
    if len == 0 {
        return Err(Error::integrity(format!("{}: empty segment", path.display())));
    }
    // `buf` always holds the file's bytes from `start` to EOF.
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    loop {
        let from = start.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (start - from) as usize];
        f.seek(SeekFrom::Start(from)).map_err(|e| io_err(path, e))?;
        f.read_exact(&mut chunk).map_err(|e| io_err(path, e))?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = from;
        if buf.last() != Some(&b'\n') {
            return Err(Error::integrity(format!("{}: torn last line (no trailing newline)", path.display())));
        }
        if let Some(pos) = buf[..buf.len() - 1].iter().rposition(|&b| b == b'\n') {
            return Ok(buf.split_off(pos + 1));
        }
        if start == 0 {
            return Ok(buf);
        }
    }
}

/// Streams one actor's claims with seq ≥ `from_seq`, ascending. Checks structure,
/// ownership and ordering; not signatures (that is `verify`).
pub(crate) struct LogIter {
    space: Id,
    actor: Id,
    from_seq: u64,
    segments: std::vec::IntoIter<Segment>,
    current: Option<Open>,
    last_seq: u64,
    done: bool,
}

struct Open {
    path: PathBuf,
    reader: BufReader<File>,
    first: u64,
    next_seq: u64,
}

impl LogIter {
    pub(crate) fn new(root: &Path, space: &Id, actor: &Id, from_seq: u64) -> Result<LogIter> {
        let all = segments(root, actor)?;
        // Segment i covers at most [first_i, first_{i+1}); skip those entirely below from_seq.
        let skip = all.windows(2).take_while(|w| w[1].first <= from_seq).count();
        let segments: Vec<Segment> = all.into_iter().skip(skip).collect();
        Ok(LogIter {
            space: space.clone(),
            actor: actor.clone(),
            from_seq,
            segments: segments.into_iter(),
            current: None,
            last_seq: 0,
            done: false,
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
                    let file = File::open(&seg.path).map_err(|e| io_err(&seg.path, e))?;
                    Open { path: seg.path, reader: BufReader::new(file), first: seg.first, next_seq: seg.first }
                }
            };
            let mut line = Vec::new();
            let n = open.reader.read_until(b'\n', &mut line).map_err(|e| io_err(&open.path, e))?;
            if n == 0 {
                if open.next_seq == open.first {
                    return Err(Error::integrity(format!("{}: empty segment", open.path.display())));
                }
                continue;
            }
            let expected = open.next_seq;
            let claim = parse_line(&line, &|| format!("{}: seq {expected}", open.path.display()))?;
            if let Some(p) = ownership_problem(&claim, &self.space, &self.actor) {
                return Err(Error::integrity(format!("{}: {p}", open.path.display())));
            }
            if claim.body.seq != expected || claim.body.seq <= self.last_seq {
                return Err(Error::integrity(format!(
                    "{}: found seq {} where seq {expected} was expected (segments must be contiguous, disjoint, and named by their first seq)",
                    open.path.display(),
                    claim.body.seq
                )));
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

impl Iterator for LogIter {
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

pub(crate) fn chain(root: &Path, space: &Id, actor: &Id) -> Result<Chain> {
    let mut out = Chain::new();
    for claim in LogIter::new(root, space, actor, 1)? {
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

/// Appends locally-authored claims: all are checked before any byte is written.
pub(crate) fn append(root: &Path, space: &Id, claims: &[Claim]) -> Result<()> {
    struct Pending {
        tail: Option<Tail>,
        first_new: u64,
        next_seq: u64,
        prev: Option<Id>,
        lines: Vec<Vec<u8>>,
    }
    let mut per_actor: BTreeMap<Id, Pending> = BTreeMap::new();
    for c in claims {
        check_claim(c, space, ErrorCode::Invalid)?;
        let pending = match per_actor.entry(c.body.actor.clone()) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => {
                let tail = tail(root, space, v.key())?;
                let (next_seq, prev) = match &tail {
                    Some(t) => (t.head.seq + 1, Some(t.head.id.clone())),
                    None => (1, None),
                };
                v.insert(Pending { tail, first_new: next_seq, next_seq, prev, lines: Vec::new() })
            }
        };
        if c.body.seq != pending.next_seq || c.body.prev != pending.prev {
            return Err(Error::conflict(format!(
                "claim {} does not continue the chain of {}: expected seq {}, got seq {}",
                c.id, c.body.actor, pending.next_seq, c.body.seq
            ))
            .with_detail(json!({
                "actor": c.body.actor,
                "expectedSeq": pending.next_seq,
                "expectedPrev": pending.prev,
                "seq": c.body.seq,
                "prev": c.body.prev,
            })));
        }
        pending.lines.push(c.to_line()?);
        pending.next_seq += 1;
        pending.prev = Some(c.id.clone());
    }
    for (actor, p) in &per_actor {
        let tail = p.tail.as_ref().map(|t| (&t.segment, t.lines));
        write_run(&actor_dir(root, actor), tail, p.first_new, &p.lines, SEGMENT_LIMIT)?;
    }
    Ok(())
}

/// Writes `lines` (consecutive seqs from `first_seq`): first into the open `tail`
/// segment while it has room under `limit`, then into new segments of ≤ `limit`
/// lines, each named by its first seq.
pub(crate) fn write_run(dir: &Path, tail: Option<(&Segment, u64)>, first_seq: u64, lines: &[Vec<u8>], limit: usize) -> Result<()> {
    let mut rest = lines;
    let mut seq = first_seq;
    if let Some((segment, count)) = tail {
        // ≤ rest.len(), so the cast back to usize is lossless.
        let room = (limit as u64).saturating_sub(count).min(rest.len() as u64) as usize;
        if room > 0 {
            append_to_segment(&segment.path, &rest[..room])?;
            rest = &rest[room..];
            seq += room as u64;
        }
    }
    for part in rest.chunks(limit.max(1)) {
        write_new_segment(dir, seq, part)?;
        seq += part.len() as u64;
    }
    Ok(())
}

fn append_to_segment(path: &Path, lines: &[Vec<u8>]) -> Result<()> {
    let mut f = OpenOptions::new().append(true).open(path).map_err(|e| io_err(path, e))?;
    f.write_all(&lines.concat()).map_err(|e| io_err(path, e))?;
    f.sync_all().map_err(|e| io_err(path, e))
}

fn write_new_segment(dir: &Path, first: u64, lines: &[Vec<u8>]) -> Result<()> {
    fsutil::ensure_dir(dir)?;
    let dest = dir.join(segment_name(first));
    // Callers hold the write lock, so no other Knowa writer can race this check.
    if dest.try_exists().map_err(|e| io_err(&dest, e))? {
        return Err(Error::integrity(format!(
            "{}: segment already exists but does not contain seq {first}",
            dest.display()
        )));
    }
    let mut tmp = TempFile::create_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    tmp.file().write_all(&lines.concat()).map_err(|e| io_err(&tmp_path, e))?;
    tmp.commit(&dest)
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
    /// Writes every planned run as new segments.
    pub(crate) fn apply(self, root: &Path) -> Result<ImportReport> {
        for (actor, first, lines) in &self.runs {
            write_run(&actor_dir(root, actor), None, *first, lines, SEGMENT_LIMIT)?;
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
    use std::fs;

    use super::*;

    fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<Vec<u8>> {
        range.map(|n| format!("{n}\n").into_bytes()).collect()
    }

    fn layout(dir: &Path) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name().into_string().unwrap(), fs::read_to_string(e.path()).unwrap())
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn write_run_fills_the_tail_then_rotates() {
        let dir = std::env::temp_dir().join(format!("knowa-store-rotate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_run(&dir, None, 1, &lines(1..=7), 3).unwrap();
        assert_eq!(
            layout(&dir),
            [
                (segment_name(1), "1\n2\n3\n".to_string()),
                (segment_name(4), "4\n5\n6\n".to_string()),
                (segment_name(7), "7\n".to_string()),
            ]
        );
        let tail = Segment { first: 7, path: dir.join(segment_name(7)) };
        write_run(&dir, Some((&tail, 1)), 8, &lines(8..=11), 3).unwrap();
        let after = layout(&dir);
        assert_eq!(after.len(), 4);
        assert_eq!(after[2], (segment_name(7), "7\n8\n9\n".to_string()));
        assert_eq!(after[3], (segment_name(10), "10\n11\n".to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

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
