//! What `Replica` hands its stores, observed through the backend seam.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use knowa_model::vocab::OWNER;
use knowa_model::{Claim, ClaimBody, Draft, Error, Id, Result, Signer, Term, Timestamp, Value};

use crate::backend::{Blobs, Guard, LastLine, Listing, Logs, Segment};
use crate::dir::{DirBlobs, DirLogs};
use crate::{Location, Manifest, Replica, Stores};

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
    fail_runs: bool,
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
        if self.fail_runs {
            self.record(format!("failed segment {first_seq}"));
            return Err(Error::internal("injected segment write failure"));
        }
        self.record(format!("segment {first_seq}"));
        self.inner.write_run(actor, first_seq, lines)
    }
}

/// The owner's chain: genesis then notes, seqs 1..=n.
fn chain(n: u64) -> (Manifest, Vec<Claim>) {
    let signer = Signer::from_seed([7; 32]);
    let (actor, space) = (signer.actor_id(), Id::new_space().unwrap());
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
    let manifest = Manifest { format: 1, space, owner: actor, genesis: claims[0].id.clone() };
    (manifest, claims)
}

fn recorded(s: &Scratch, manifest: &Manifest, fail_runs: bool) -> (Replica, Arc<Mutex<Vec<String>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let logs = Recording { inner: DirLogs::new(&s.0), events: Arc::clone(&events), fail_runs };
    let blobs = DirBlobs::new(&s.0);
    logs.prepare().unwrap();
    blobs.prepare().unwrap();
    let stores = Stores { location: Location::Dir(s.0.clone()), logs: Box::new(logs), blobs: Box::new(blobs) };
    (Replica::with_stores(stores, manifest.clone()), events)
}

fn take(events: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *events.lock().unwrap())
}

/// DESIGN §5.4: catch-up cannot see segments that fill gaps below an indexed head, so a
/// merge writes a fresh generation before its first segment and again after its last.
#[test]
fn merge_writes_the_generation_before_its_first_segment_and_after_its_last() {
    let s = Scratch::new("merge-generation");
    let (manifest, c) = chain(6);
    let (r, events) = recorded(&s, &manifest, false);

    r.merge(&[c[0].clone(), c[1].clone(), c[4].clone()]).unwrap();
    assert_eq!(take(&events), ["generation", "segment 1", "segment 5", "generation"]);
    let first = r.generation().unwrap().unwrap();

    // Filling the gap below the head: the case the generation exists for.
    r.merge(&[c[2].clone(), c[3].clone()]).unwrap();
    assert_eq!(take(&events), ["generation", "segment 3", "generation"]);
    assert_ne!(r.generation().unwrap().unwrap(), first);

    // Nothing new: nothing written, the generation stays.
    let now = r.generation().unwrap();
    assert_eq!(r.merge(&c[..5]).unwrap().skipped_existing, 5);
    assert!(take(&events).is_empty());
    assert_eq!(r.generation().unwrap(), now);
}

/// What a failed merge wrote stays (each run is verified and chain-checked), so the
/// generation is written after it too.
#[test]
fn a_merge_that_fails_part_way_still_writes_the_generation_after() {
    let s = Scratch::new("merge-generation-fail");
    let (manifest, c) = chain(2);
    let (r, events) = recorded(&s, &manifest, true);
    let err = r.merge(&c).unwrap_err();
    assert_eq!(err.message, "injected segment write failure");
    assert_eq!(take(&events), ["generation", "failed segment 1", "generation"]);
    assert!(r.generation().unwrap().is_some());
}

/// DESIGN §5.2: b32(sha256(location)), lowercase and unpadded, first 16 characters.
/// Vectors from an independent RFC 4648 encoder (Python's `base64.b32encode`).
#[test]
fn location_keys_are_the_documented_prefix_of_the_digest() {
    assert_eq!(crate::location_key("/srv/knowa/notes"), "qxsht3qtnfdghoub");
    assert_eq!(crate::location_key(""), "4oymiquy7qobjgx3");
}
