//! What `Replica` hands its stores, observed through the backend seam.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use knowa_model::vocab::OWNER;
use knowa_model::{Claim, ClaimBody, Draft, Error, Id, Result, Signer, Term, Timestamp, Value};

use crate::backend::{Blobs, Guard, LastLine, Listing, Logs, Segment};
use crate::dir::{DirBlobs, DirLogs};
use crate::{ImportReport, Location, Manifest, Replica, Stores};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("knowa-store-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Directory logs that record, in order, the generation and segment writes they get,
/// and fail every segment write while `fail_runs` is set.
struct Recording {
    inner: DirLogs,
    events: Arc<Mutex<Vec<String>>>,
    fail_runs: Arc<AtomicBool>,
}

impl Recording {
    fn record(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
}

impl Logs for Recording {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn exclusive_locks(&self) -> bool {
        self.inner.exclusive_locks()
    }
    fn prepare(&self) -> Result<()> {
        self.inner.prepare()
    }
    fn read_manifest(&self) -> Result<Option<Vec<u8>>> {
        self.inner.read_manifest()
    }
    fn write_manifest(&self, bytes: &[u8]) -> Result<()> {
        self.inner.write_manifest(bytes)
    }
    fn read_generation(&self) -> Result<Option<Vec<u8>>> {
        self.inner.read_generation()
    }
    fn write_generation(&self, bytes: &[u8]) -> Result<()> {
        self.record("generation".into());
        self.inner.write_generation(bytes)
    }
    fn read_gaps(&self) -> Result<Option<Vec<u8>>> {
        self.inner.read_gaps()
    }
    fn write_gaps(&self, bytes: &[u8]) -> Result<()> {
        self.record("gaps".into());
        self.inner.write_gaps(bytes)
    }
    fn lock_exclusive(&self, actor: &Id) -> Result<Guard> {
        self.inner.lock_exclusive(actor)
    }
    fn lock_shared(&self, actor: &Id) -> Result<Option<Guard>> {
        self.inner.lock_shared(actor)
    }
    fn repair_tail(&self, actor: &Id) -> Result<()> {
        self.inner.repair_tail(actor)
    }
    fn actors(&self) -> Result<Listing<Id>> {
        self.inner.actors()
    }
    fn segments(&self, actor: &Id) -> Result<Listing<Segment>> {
        self.inner.segments(actor)
    }
    fn open_segment(&self, actor: &Id, segment: &Segment) -> Result<Box<dyn BufRead + Send>> {
        self.inner.open_segment(actor, segment)
    }
    fn last_line(&self, actor: &Id, segment: &Segment) -> Result<LastLine> {
        self.inner.last_line(actor, segment)
    }
    fn append(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        self.inner.append(actor, first_seq, lines)
    }
    fn write_run(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        if self.fail_runs.load(Ordering::SeqCst) {
            self.record(format!("failed segment {first_seq}"));
            return Err(Error::internal("injected segment write failure"));
        }
        self.record(format!("segment {first_seq}"));
        self.inner.write_run(actor, first_seq, lines)
    }
}

/// A chain of `n` claims by the actor of `seed` in `space`, seqs 1..=n; seq 1 is a
/// genesis.
fn chain_of(seed: u8, space: &Id, n: u64) -> Vec<Claim> {
    let signer = Signer::from_seed([seed; 32]);
    let actor = signer.actor_id();
    let mut claims: Vec<Claim> = Vec::new();
    for seq in 1..=n {
        let d = if seq == 1 {
            Draft::new(space.clone(), Term::parse(OWNER).unwrap(), Value::Ref(actor.clone()))
        } else {
            Draft::new(Id::new_entity().unwrap(), Term::parse("test:note").unwrap(), Value::text(format!("n{seq}")))
        };
        let prev = claims.last().map(|c| c.id.clone());
        let body = ClaimBody::from_draft(d, space.clone(), actor.clone(), seq, prev, Timestamp::now());
        claims.push(signer.seal(body).unwrap());
    }
    claims
}

/// The owner's chain in a new space: genesis then notes, seqs 1..=n.
fn chain(n: u64) -> (Manifest, Vec<Claim>) {
    let claims = chain_of(7, &Id::new_space().unwrap(), n);
    let b = &claims[0].body;
    let manifest = Manifest { format: 1, space: b.space.clone(), owner: b.actor.clone(), genesis: claims[0].id.clone() };
    (manifest, claims)
}

/// A recording replica over a fresh directory, its events, and the switch that makes
/// its segment writes fail.
struct Recorded {
    replica: Replica,
    events: Arc<Mutex<Vec<String>>>,
    fail_runs: Arc<AtomicBool>,
}

impl Recorded {
    fn new(s: &Scratch, manifest: &Manifest) -> Recorded {
        let events = Arc::new(Mutex::new(Vec::new()));
        let fail_runs = Arc::new(AtomicBool::new(false));
        let logs = Recording { inner: DirLogs::new(&s.0), events: Arc::clone(&events), fail_runs: Arc::clone(&fail_runs) };
        let blobs = DirBlobs::new(&s.0);
        logs.prepare().unwrap();
        blobs.prepare().unwrap();
        let stores = Stores { location: Location::Dir(s.0.clone()), logs: Box::new(logs), blobs: Box::new(blobs) };
        Recorded { replica: Replica::with_stores(stores, manifest.clone()), events, fail_runs }
    }

    fn merge(&self, claims: &[&Claim]) -> Result<ImportReport> {
        self.replica.merge(&claims.iter().map(|c| (*c).clone()).collect::<Vec<_>>())
    }

    /// The events recorded since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    /// (`generation`, `gaps`)
    fn generations(&self) -> (Option<String>, Option<String>) {
        (self.replica.generation().unwrap(), self.replica.gaps().unwrap())
    }
}

/// DESIGN §5.4: claims above every head need nothing, since catch-up sees them; a claim
/// at or below its actor's head fills a gap, and gets a fresh `gaps` and `generation`
/// before the merge's first segment and both again after its last, the generation
/// first.
#[test]
fn only_a_merge_that_fills_a_gap_writes_the_generations_around_its_segments() {
    let s = Scratch::new("merge-generations");
    let (manifest, c) = chain(7);
    let r = Recorded::new(&s, &manifest);

    // Into an empty log: every claim is above its head, 0, gaps between them and all.
    r.merge(&[&c[0], &c[1], &c[4]]).unwrap();
    assert_eq!(r.take(), ["segment 1", "segment 5"]);
    assert_eq!(r.generations(), (None, None));

    // Below the head, 5: the case the generations exist for.
    r.merge(&[&c[2], &c[3]]).unwrap();
    assert_eq!(r.take(), ["gaps", "generation", "segment 3", "generation", "gaps"]);
    let filled = r.generations();
    assert!(filled.0.is_some() && filled.1.is_some(), "{filled:?}");

    // Above the head again: nothing but the segment, and both generations stay.
    r.merge(&[&c[6]]).unwrap();
    assert_eq!(r.take(), ["segment 7"]);
    assert_eq!(r.generations(), filled);

    // One claim below the head among others above it is enough.
    let other = chain_of(8, &manifest.space, 1);
    r.merge(&[&c[5], &other[0]]).unwrap();
    assert_eq!(r.take(), ["gaps", "generation", "segment 6", "segment 1", "generation", "gaps"]);
    let (generation, gaps) = r.generations();
    assert!(generation != filled.0 && gaps != filled.1, "both are fresh");

    // Nothing new: nothing written.
    assert_eq!(r.replica.merge(&c).unwrap().skipped_existing, 7);
    assert!(r.take().is_empty());
    assert_eq!(r.generations(), (generation, gaps));
}

/// DESIGN §5.4: an added blob may carry text for claims already indexed, so it needs a
/// fresh `generation` before the merge's first write, the blobs, and after its last;
/// it fills no gap, so `gaps` stays.
#[test]
fn a_merge_that_adds_blobs_writes_only_the_rebuild_generation() {
    let s = Scratch::new("merge-blobs");
    let (manifest, c) = chain(2);
    let r = Recorded::new(&s, &manifest);
    let events = Arc::clone(&r.events);
    let put_blobs = || {
        events.lock().unwrap().push("blobs".into());
        Ok(1)
    };
    let report = r.replica.merge_checked(&c, true, put_blobs).unwrap();
    assert_eq!((report.added, report.blobs_added), (2, 1));
    assert_eq!(r.take(), ["generation", "blobs", "segment 1", "generation"]);
    let (generation, gaps) = r.generations();
    assert!(generation.is_some());
    assert_eq!(gaps, None);
}

/// What a failed merge wrote stays (each run is verified and chain-checked), so the
/// generations it needed are written after it too; one that needed none writes none.
#[test]
fn a_merge_that_fails_part_way_still_writes_its_generations_after() {
    let s = Scratch::new("merge-generations-fail");
    let (manifest, c) = chain(4);
    let r = Recorded::new(&s, &manifest);
    r.merge(&[&c[0], &c[1], &c[3]]).unwrap();
    r.take();
    r.fail_runs.store(true, Ordering::SeqCst);

    let err = r.merge(&[&c[2]]).unwrap_err();
    assert_eq!(err.message, "injected segment write failure");
    assert_eq!(r.take(), ["gaps", "generation", "failed segment 3", "generation", "gaps"]);
    let (generation, gaps) = r.generations();
    assert!(generation.is_some() && gaps.is_some());

    let other = chain_of(8, &manifest.space, 1);
    let err = r.merge(&[&other[0]]).unwrap_err();
    assert_eq!(err.message, "injected segment write failure");
    assert_eq!(r.take(), ["failed segment 1"], "a claim of an actor without a log fills no gap");
    assert_eq!(r.generations(), (generation, gaps));
}

/// DESIGN §5.2: b32(sha256(location)), lowercase and unpadded, first 16 characters.
/// Vectors from an independent RFC 4648 encoder (Python's `base64.b32encode`).
#[test]
fn location_keys_are_the_documented_prefix_of_the_digest() {
    assert_eq!(crate::location_key("/srv/knowa/notes"), "qxsht3qtnfdghoub");
    assert_eq!(crate::location_key(""), "4oymiquy7qobjgx3");
}
