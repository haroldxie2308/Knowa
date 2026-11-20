//! Replica integrity: chain/append/merge (gaps, equivocation), blobs, verify,
//! bundle round-trips and tamper rejection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use knowa_model::vocab::OWNER;
use knowa_model::{Claim, ClaimBody, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value};
use knowa_store::{BundleMeta, Head, Manifest, Replica, import_bundle, write_bundle};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Scratch(std::env::temp_dir().join(format!("knowa-store-{tag}-{}-{n}", std::process::id())))
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An actor writing a hash chain in one space.
struct Author {
    signer: Signer,
    id: Id,
    space: Id,
    last: Option<(u64, Id)>,
}

impl Author {
    fn new(space: &Id) -> Author {
        let signer = Signer::generate().unwrap();
        let id = signer.actor_id();
        Author { signer, id, space: space.clone(), last: None }
    }

    fn seal_at(&self, seq: u64, prev: Option<Id>, d: Draft) -> Claim {
        let body = ClaimBody::from_draft(d, self.space.clone(), self.id.clone(), seq, prev, Timestamp::now());
        self.signer.seal(body).unwrap()
    }

    fn push(&mut self, d: Draft) -> Claim {
        let (seq, prev) = match &self.last {
            Some((s, id)) => (s + 1, Some(id.clone())),
            None => (1, None),
        };
        let c = self.seal_at(seq, prev, d);
        self.last = Some((seq, c.id.clone()));
        c
    }

    fn genesis(&mut self) -> Claim {
        let d = Draft::new(self.space.clone(), Term::parse(OWNER).unwrap(), Value::Ref(self.id.clone()));
        self.push(d)
    }

    fn note(&mut self, text: &str) -> Claim {
        self.push(note(text))
    }
}

fn note(text: &str) -> Draft {
    Draft::new(Id::new_entity().unwrap(), Term::parse("test:note").unwrap(), Value::text(text))
}

fn new_space(dir: &Path) -> (Replica, Author, Claim) {
    let mut owner = Author::new(&Id::new_space().unwrap());
    let genesis = owner.genesis();
    (Replica::create(dir, &genesis).unwrap(), owner, genesis)
}

fn log_of(r: &Replica, actor: &Id, from: u64) -> Vec<Claim> {
    r.read_log(actor, from).unwrap().collect::<Result<_, _>>().unwrap()
}

fn seqs(claims: &[Claim]) -> Vec<u64> {
    claims.iter().map(|c| c.body.seq).collect()
}

fn segment_files(r: &Replica, actor: &Id) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(r.root().join("log").join(actor.as_str()))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn meta_for(r: &Replica, level: &str, by: &Id) -> BundleMeta {
    let m = r.manifest();
    BundleMeta {
        format: m.format,
        space: m.space.clone(),
        owner: m.owner.clone(),
        genesis: m.genesis.clone(),
        level: level.to_string(),
        partial: level != "private",
        exported_by: by.clone(),
        at: Timestamp::now().format(),
    }
}

fn replace_once(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    assert_eq!(needle.len(), with.len());
    let at = haystack.windows(needle.len()).position(|w| w == needle).expect("needle present");
    let mut out = haystack.to_vec();
    out[at..at + needle.len()].copy_from_slice(with);
    out
}

#[test]
fn create_then_open_and_verify() {
    let s = Scratch::new("create");
    let (r, owner, genesis) = new_space(&s.0);
    let expected =
        Manifest { format: 1, space: owner.space.clone(), owner: owner.id.clone(), genesis: genesis.id.clone() };
    assert_eq!(r.manifest(), &expected);
    assert_eq!(Replica::open(&s.0).unwrap().manifest(), &expected);

    let lock = r.lock().unwrap();
    assert_eq!(r.head(&lock, &owner.id).unwrap(), Some(Head { seq: 1, id: genesis.id.clone() }));
    let line = fs::read(s.path(&format!("log/{}/000000000001.jsonl", owner.id))).unwrap();
    assert_eq!(line, genesis.to_line().unwrap());

    let report = r.verify().unwrap();
    assert_eq!((report.claims, report.actors, report.blobs), (1, 1, 0));
    assert!(report.gaps.is_empty());
}

#[test]
fn create_and_open_fail_fast() {
    let s = Scratch::new("create-bad");
    fs::create_dir_all(&s.0).unwrap();
    fs::write(s.path("stray"), b"x").unwrap();
    let mut owner = Author::new(&Id::new_space().unwrap());
    let genesis = owner.genesis();
    assert_eq!(Replica::create(&s.0, &genesis).err().unwrap().code, ErrorCode::Conflict);

    let not_genesis = Author::new(&owner.space).note("not an owner claim");
    assert_eq!(Replica::create(&s.path("fresh"), &not_genesis).err().unwrap().code, ErrorCode::Invalid);

    assert_eq!(Replica::open(&s.path("missing")).err().unwrap().code, ErrorCode::NotFound);
}

#[test]
fn append_continues_the_chain_and_writes_nothing_on_error() {
    let s = Scratch::new("append");
    let (r, mut owner, _) = new_space(&s.0);
    let lock = r.lock().unwrap();
    let c2 = owner.note("two");
    let c3 = owner.note("three");
    r.append(&lock, &[c2.clone(), c3.clone()]).unwrap();
    assert_eq!(log_of(&r, &owner.id, 2), vec![c2, c3.clone()]);

    // A valid claim followed by one that skips a seq: the batch is refused whole.
    let c4 = owner.note("four");
    let _c5 = owner.note("five");
    let c6 = owner.note("six");
    assert_eq!(r.append(&lock, &[c4.clone(), c6]).unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(r.head(&lock, &owner.id).unwrap().unwrap().seq, 3);

    // A forged claim fails verification; again nothing is written.
    let mut forged = c4.clone();
    forged.body.note = Some("forged".into());
    assert_eq!(r.append(&lock, &[forged]).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(r.head(&lock, &owner.id).unwrap().unwrap().seq, 3);

    // A new actor's first claim must be seq 1.
    let mut other = Author::new(&owner.space);
    let o1 = other.note("hello");
    let o2 = other.note("again");
    assert_eq!(r.append(&lock, std::slice::from_ref(&o2)).unwrap_err().code, ErrorCode::Conflict);
    r.append(&lock, &[c4, o1, o2]).unwrap();
    assert_eq!(r.head(&lock, &owner.id).unwrap().unwrap().seq, 4);
    assert_eq!(r.head(&lock, &other.id).unwrap().unwrap().seq, 2);
    let mut actors = vec![owner.id.clone(), other.id.clone()];
    actors.sort();
    assert_eq!(r.actors().unwrap(), actors);
    r.verify().unwrap();
}

#[test]
fn merge_tolerates_gaps_fills_them_and_appends_after() {
    let sa = Scratch::new("merge-src");
    let (a, mut owner, _) = new_space(&sa.0);
    let la = a.lock().unwrap();
    let more: Vec<Claim> = (2..=6).map(|i| owner.note(&format!("n{i}"))).collect();
    a.append(&la, &more).unwrap();
    let all = log_of(&a, &owner.id, 1);
    assert_eq!(seqs(&all), [1, 2, 3, 4, 5, 6]);

    let sb = Scratch::new("merge-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    let lb = b.lock().unwrap();
    let partial = [all[0].clone(), all[1].clone(), all[4].clone(), all[5].clone()];
    let rep = b.merge(&lb, &partial).unwrap();
    assert_eq!((rep.added, rep.skipped_existing), (4, 0));
    assert_eq!(rep.gaps, vec![(owner.id.clone(), 3, 4)]);
    assert_eq!(seqs(&log_of(&b, &owner.id, 1)), [1, 2, 5, 6]);
    assert_eq!(b.verify().unwrap().gaps, vec![(owner.id.clone(), 3, 4)]);

    // Fill the gap: its run becomes its own segment, and reads stay in seq order.
    let rep = b.merge(&lb, &[all[3].clone(), all[2].clone()]).unwrap();
    assert_eq!(rep.added, 2);
    assert!(rep.gaps.is_empty());
    assert_eq!(log_of(&b, &owner.id, 1), all);
    assert_eq!(seqs(&log_of(&b, &owner.id, 4)), [4, 5, 6]);
    assert_eq!(
        segment_files(&b, &owner.id),
        ["000000000001.jsonl", "000000000003.jsonl", "000000000005.jsonl"]
    );

    let rep = b.merge(&lb, &all).unwrap();
    assert_eq!((rep.added, rep.skipped_existing), (0, 6));

    // A local append continues from the highest seq.
    let c7 = owner.note("seven");
    b.append(&lb, std::slice::from_ref(&c7)).unwrap();
    assert_eq!(seqs(&log_of(&b, &owner.id, 6)), [6, 7]);
    let report = b.verify().unwrap();
    assert_eq!(report.claims, 7);
    assert!(report.gaps.is_empty());
}

#[test]
fn merge_rejects_equivocation() {
    let sa = Scratch::new("equiv-src");
    let (a, mut owner, genesis) = new_space(&sa.0);
    let la = a.lock().unwrap();
    let c2 = owner.note("honest");
    a.append(&la, std::slice::from_ref(&c2)).unwrap();
    let fork = owner.seal_at(2, Some(genesis.id.clone()), note("fork"));
    assert_ne!(fork.id, c2.id);

    let sb = Scratch::new("equiv-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    let lb = b.lock().unwrap();
    b.merge(&lb, &[genesis.clone(), c2.clone()]).unwrap();
    assert_eq!(b.merge(&lb, std::slice::from_ref(&fork)).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(log_of(&b, &owner.id, 1), vec![genesis.clone(), c2.clone()]);

    // Both branches in one batch: refused before anything is written.
    let sc = Scratch::new("equiv-batch");
    let c = Replica::create_replica(&sc.0, a.manifest()).unwrap();
    let lc = c.lock().unwrap();
    assert_eq!(c.merge(&lc, &[genesis, c2, fork]).unwrap_err().code, ErrorCode::Integrity);
    assert!(log_of(&c, &owner.id, 1).is_empty());
}

#[test]
fn merge_rejects_claims_that_do_not_link() {
    let sa = Scratch::new("link-src");
    let (a, mut owner, genesis) = new_space(&sa.0);
    let la = a.lock().unwrap();
    let c2 = owner.note("two");
    let c3 = owner.note("three");
    a.append(&la, &[c2.clone(), c3.clone()]).unwrap();

    let sb = Scratch::new("link-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    let lb = b.lock().unwrap();
    b.merge(&lb, &[genesis.clone(), c3.clone()]).unwrap();

    // A different seq-2 claim chains onto genesis but not into the existing seq 3.
    let impostor = owner.seal_at(2, Some(genesis.id.clone()), note("impostor"));
    assert_eq!(b.merge(&lb, &[impostor]).unwrap_err().code, ErrorCode::Integrity);
    // A seq-4 claim whose prev is not seq 3.
    let stray = owner.seal_at(4, Some(genesis.id.clone()), note("stray"));
    assert_eq!(b.merge(&lb, &[stray]).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(seqs(&log_of(&b, &owner.id, 1)), [1, 3]);

    // The genuine claim fills the gap.
    assert_eq!(b.merge(&lb, &[c2]).unwrap().added, 1);
    b.verify().unwrap();
}

#[test]
fn merge_rejects_foreign_and_forged_claims() {
    let sa = Scratch::new("foreign");
    let (a, mut owner, _) = new_space(&sa.0);
    let la = a.lock().unwrap();
    let mut elsewhere = Author::new(&Id::new_space().unwrap());
    let foreign = elsewhere.note("other space");
    assert_eq!(a.merge(&la, &[foreign]).unwrap_err().code, ErrorCode::Integrity);
    let mut forged = owner.note("real");
    forged.body.note = Some("edited after signing".into());
    assert_eq!(a.merge(&la, &[forged]).unwrap_err().code, ErrorCode::Integrity);
}

#[test]
fn blobs_are_deduplicated_and_content_addressed() {
    let s = Scratch::new("blobs");
    let (r, _, _) = new_space(&s.0);
    let data = b"hello blob";
    let (id, created, size) = r.put_blob(&mut &data[..]).unwrap();
    assert_eq!((created, size), (true, data.len() as u64));
    assert_eq!(id, Id::blob_of(data));
    let (again, created, _) = r.put_blob(&mut &data[..]).unwrap();
    assert_eq!((again, created), (id.clone(), false));
    assert_eq!(fs::read(r.blob_path(&id).unwrap()).unwrap(), data);
    assert!(r.has_blob(&Id::parse(&format!("{id}#page=1")).unwrap()));

    // Larger than one copy buffer.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (big_id, _, big_size) = r.put_blob(&mut big.as_slice()).unwrap();
    assert_eq!((big_id.clone(), big_size), (Id::blob_of(&big), big.len() as u64));

    let mut ids = vec![id, big_id];
    ids.sort();
    assert_eq!(r.blob_ids().unwrap(), ids);
    let leftovers: Vec<_> = fs::read_dir(s.path("blobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    let absent = Id::blob_of(b"never stored");
    assert!(!r.has_blob(&absent));
    assert_eq!(r.blob_path(&absent).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn verify_reports_tampered_logs_and_blobs() {
    let s = Scratch::new("verify");
    let (r, mut owner, _) = new_space(&s.0);
    let lock = r.lock().unwrap();
    r.append(&lock, &[owner.note("original text")]).unwrap();
    let (blob, _, _) = r.put_blob(&mut &b"blob bytes"[..]).unwrap();
    r.verify().unwrap();

    let segment = s.path(&format!("log/{}/000000000001.jsonl", owner.id));
    let pristine = fs::read(&segment).unwrap();
    fs::write(&segment, replace_once(&pristine, b"original text", b"0riginal text")).unwrap();
    let err = r.verify().unwrap_err();
    assert_eq!(err.code, ErrorCode::Integrity);
    assert!(!err.detail.unwrap()["problems"].as_array().unwrap().is_empty());
    fs::write(&segment, &pristine).unwrap();
    r.verify().unwrap();

    fs::write(r.blob_path(&blob).unwrap(), b"blob bytez").unwrap();
    assert_eq!(r.verify().unwrap_err().code, ErrorCode::Integrity);
}

/// A replica with three claims beyond genesis, one referencing a blob, plus two blobs.
fn populated(dir: &Path) -> (Replica, Author, Vec<Claim>, Vec<Id>) {
    let (r, mut owner, _) = new_space(dir);
    let lock = r.lock().unwrap();
    let (b1, _, _) = r.put_blob(&mut &b"BLOB-PAYLOAD-ONE"[..]).unwrap();
    let (b2, _, _) = r.put_blob(&mut &b"BLOB-PAYLOAD-TWO"[..]).unwrap();
    let content = Draft::new(Id::new_entity().unwrap(), Term::parse("knowa:content").unwrap(), Value::Ref(b1.clone()));
    let claims = vec![owner.push(content), owner.note("note-alpha"), owner.note("note-omega")];
    r.append(&lock, &claims).unwrap();
    drop(lock);
    let all = log_of(&r, &owner.id, 1);
    (r, owner, all, vec![b1, b2])
}

#[test]
fn bundle_round_trip_is_deterministic_and_idempotent() {
    let s = Scratch::new("bundle");
    let (a, owner, claims, blobs) = populated(&s.path("a"));
    let meta = meta_for(&a, "private", &owner.id);
    write_bundle(&a, &s.path("x.knowa"), &meta, &claims, &blobs).unwrap();
    write_bundle(&a, &s.path("y.knowa"), &meta, &claims, &blobs).unwrap();
    assert_eq!(fs::read(s.path("x.knowa")).unwrap(), fs::read(s.path("y.knowa")).unwrap());

    let (b, got_meta, rep) = import_bundle(&s.path("x.knowa"), &s.path("b")).unwrap();
    assert_eq!(got_meta, meta);
    assert_eq!(b.manifest(), a.manifest());
    assert_eq!((rep.added, rep.skipped_existing, rep.blobs_added), (4, 0, 2));
    assert!(rep.gaps.is_empty());
    assert_eq!(log_of(&b, &owner.id, 1), claims);
    for id in &blobs {
        assert_eq!(fs::read(b.blob_path(id).unwrap()).unwrap(), fs::read(a.blob_path(id).unwrap()).unwrap());
    }
    b.verify().unwrap();

    let (_, _, rep) = import_bundle(&s.path("x.knowa"), &s.path("b")).unwrap();
    assert_eq!((rep.added, rep.skipped_existing, rep.blobs_added), (0, 4, 0));
}

#[test]
fn partial_bundle_imports_with_gaps() {
    let s = Scratch::new("partial");
    let (a, owner, claims, _) = populated(&s.path("a"));
    let subset = [claims[0].clone(), claims[1].clone(), claims[3].clone()];
    write_bundle(&a, &s.path("p.knowa"), &meta_for(&a, "shared", &owner.id), &subset, &[]).unwrap();
    let (b, meta, rep) = import_bundle(&s.path("p.knowa"), &s.path("b")).unwrap();
    assert!(meta.partial);
    assert_eq!(rep.added, 3);
    assert_eq!(rep.gaps, vec![(owner.id.clone(), 3, 3)]);
    assert_eq!(seqs(&log_of(&b, &owner.id, 1)), [1, 2, 4]);
    assert_eq!(b.verify().unwrap().gaps, vec![(owner.id.clone(), 3, 3)]);
}

#[test]
fn tampered_bundles_are_rejected_before_writing() {
    let s = Scratch::new("tamper");
    let (a, owner, claims, blobs) = populated(&s.path("a"));
    write_bundle(&a, &s.path("ok.knowa"), &meta_for(&a, "private", &owner.id), &claims, &blobs).unwrap();
    let bytes = fs::read(s.path("ok.knowa")).unwrap();

    fs::write(s.path("blob.knowa"), replace_once(&bytes, b"BLOB-PAYLOAD-TWO", b"BLOB-PAYLOAD-TW0")).unwrap();
    assert_eq!(import_bundle(&s.path("blob.knowa"), &s.path("d1")).err().unwrap().code, ErrorCode::Integrity);
    assert!(!s.path("d1").exists());

    fs::write(s.path("claim.knowa"), replace_once(&bytes, b"note-omega", b"note-0mega")).unwrap();
    assert_eq!(import_bundle(&s.path("claim.knowa"), &s.path("d2")).err().unwrap().code, ErrorCode::Integrity);
    assert!(!s.path("d2").exists());

    // Into an existing replica: nothing changes there either.
    let (b, _, _) = import_bundle(&s.path("ok.knowa"), &s.path("d3")).unwrap();
    let before = log_of(&b, &owner.id, 1);
    assert_eq!(import_bundle(&s.path("claim.knowa"), &s.path("d3")).err().unwrap().code, ErrorCode::Integrity);
    assert_eq!(log_of(&b, &owner.id, 1), before);
}

#[test]
fn import_into_a_different_space_is_a_conflict() {
    let s = Scratch::new("mismatch");
    let (a, owner, claims, blobs) = populated(&s.path("a"));
    write_bundle(&a, &s.path("x.knowa"), &meta_for(&a, "private", &owner.id), &claims, &blobs).unwrap();
    new_space(&s.path("other"));
    assert_eq!(import_bundle(&s.path("x.knowa"), &s.path("other")).err().unwrap().code, ErrorCode::Conflict);
    assert_eq!(import_bundle(&s.path("missing.knowa"), &s.path("z")).err().unwrap().code, ErrorCode::NotFound);
}

#[test]
fn blob_fanout_spreads_by_digest_and_lock_is_bound_to_its_replica() {
    let scratch = Scratch::new("fanout");
    let (a, _, _) = new_space(&scratch.path("a"));
    let (b, _, _) = new_space(&scratch.path("b"));
    let mut dirs = std::collections::BTreeSet::new();
    for i in 0..32u32 {
        let (id, _, _) = a.put_blob(&mut i.to_le_bytes().as_slice()).unwrap();
        let p = a.blob_path(&id).unwrap();
        dirs.insert(p.parent().unwrap().file_name().unwrap().to_owned());
        assert_eq!(&id.as_str()[6..8], p.parent().unwrap().file_name().unwrap().to_str().unwrap());
    }
    assert!(dirs.len() > 8, "32 blobs landed in only {} fan-out dirs", dirs.len());
    let lock_a = a.lock().unwrap();
    let err = b.head(&lock_a, &b.manifest().owner).unwrap_err();
    assert_eq!(err.code, knowa_model::ErrorCode::Internal);
}
