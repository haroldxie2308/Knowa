//! Full replica check (`knowa verify`). Collects every problem instead of stopping
//! at the first; only I/O failures abort.

use std::collections::btree_map::Entry;
use std::collections::BTreeSet;
use std::fs;

use knowa_model::{Error, Result};

use crate::fsutil::io_err;
use crate::log::{self, Chain};
use crate::{Replica, VerifyReport, blob};

pub(crate) fn verify(replica: &Replica) -> Result<VerifyReport> {
    let root = replica.root();
    let m = replica.manifest();
    let mut rep = VerifyReport::default();

    let (actors, problems) = log::scan_actors(root)?;
    rep.problems.extend(problems);
    rep.actors = actors.len() as u64;
    let mut owner_seq1 = None;

    for actor in &actors {
        let (segments, problems) = log::scan_segments(root, actor)?;
        rep.problems.extend(problems);
        let mut chain = Chain::new();
        for seg in &segments {
            let bytes = fs::read(&seg.path).map_err(|e| io_err(&seg.path, e))?;
            if bytes.is_empty() {
                rep.problems.push(format!("{}: empty segment", seg.path.display()));
                continue;
            }
            for (i, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
                let lineno = i + 1;
                let loc = || format!("{}:{lineno}", seg.path.display());
                let claim = match log::parse_line(line, &loc) {
                    Ok(c) => c,
                    Err(e) => {
                        rep.problems.push(e.message);
                        continue;
                    }
                };
                rep.claims += 1;
                match claim.to_line() {
                    Ok(canonical) if canonical == line => {}
                    Ok(_) => rep.problems.push(format!("{}: claim {} is not in canonical (JCS) form", loc(), claim.id)),
                    Err(e) => rep.problems.push(format!("{}: claim {}: {}", loc(), claim.id, e.message)),
                }
                if let Err(e) = claim.verify() {
                    rep.problems.push(format!("{}: claim {}: {}", loc(), claim.id, e.message));
                }
                if claim.body.space != m.space {
                    rep.problems.push(format!("{}: claim {} belongs to space {}", loc(), claim.id, claim.body.space));
                }
                if claim.body.actor != *actor {
                    rep.problems.push(format!("{}: claim {} is by {}", loc(), claim.id, claim.body.actor));
                }
                let expected = seg.first + i as u64;
                if claim.body.seq != expected {
                    rep.problems.push(format!(
                        "{}: seq {} where {expected} was expected (segments are contiguous and named by their first seq)",
                        loc(),
                        claim.body.seq
                    ));
                }
                if *actor == m.owner && claim.body.seq == 1 {
                    owner_seq1 = Some(claim.id.clone());
                }
                match chain.entry(claim.body.seq) {
                    Entry::Vacant(v) => {
                        v.insert((claim.id, claim.body.prev));
                    }
                    Entry::Occupied(o) => rep.problems.push(if o.get().0 == claim.id {
                        format!("{}: claim {} appears twice", loc(), claim.id)
                    } else {
                        format!("{}: equivocation: seq {} is both {} and {}", loc(), o.key(), o.get().0, claim.id)
                    }),
                }
            }
        }
        for (&seq, (id, prev)) in &chain {
            if let Some((below, _)) = seq.checked_sub(1).and_then(|s| chain.get(&s))
                && prev.as_ref() != Some(below)
            {
                rep.problems.push(format!("broken chain: {actor} seq {seq} ({id}) does not link to seq {} ({below})", seq - 1));
            }
        }
        let seqs: BTreeSet<u64> = chain.keys().copied().collect();
        rep.gaps.extend(log::gaps(&seqs).into_iter().map(|(lo, hi)| (actor.clone(), lo, hi)));
    }

    match owner_seq1 {
        Some(id) if id == m.genesis => {}
        Some(id) => rep.problems.push(format!("owner {} seq 1 is {id}, but the manifest genesis is {}", m.owner, m.genesis)),
        None => rep.problems.push(format!("genesis claim {} (seq 1 of owner {}) is missing", m.genesis, m.owner)),
    }

    let (blobs, problems) = blob::scan(root)?;
    rep.problems.extend(problems);
    rep.blobs = blobs.len() as u64;
    for (id, path) in &blobs {
        if let Some(p) = blob::check(id, path)? {
            rep.problems.push(p);
        }
    }

    if rep.problems.is_empty() {
        return Ok(rep);
    }
    let message = format!("verify found {} problem(s); first: {}", rep.problems.len(), rep.problems[0]);
    Err(Error::integrity(message).with_detail(serde_json::to_value(&rep)?))
}
