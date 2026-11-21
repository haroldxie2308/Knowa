//! Replica integrity: chain/append/merge (gaps, equivocation), per-actor locks, the
//! rebuild generation, blobs, verify, bundle round-trips and tamper rejection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use knowa_model::vocab::OWNER;
use knowa_model::{Claim, ClaimBody, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value};
use knowa_store::{BundleMeta, Head, Manifest, Replica, hash_file, import_bundle, write_bundle};

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

/// Segment file names; dot-names (the actor's `.lock`, temp files) are not segments.
fn segment_files(r: &Replica, actor: &Id) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(r.root().join("log").join(actor.as_str()))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.starts_with('.'))
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

fn segment_path(r: &Replica, actor: &Id, first: u64) -> PathBuf {
    r.root().join("log").join(actor.as_str()).join(format!("{first:012}.jsonl"))
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}

/// An author different from `than` whose id sorts after it.
fn author_after(space: &Id, than: &Id) -> Author {
    loop {
        let a = Author::new(space);
        if a.id > *than {
            return a;
        }
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

    let lock = r.lock_actor(&owner.id).unwrap();
    assert_eq!(r.head(&lock).unwrap(), Some(Head { seq: 1, id: genesis.id.clone() }));
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
    let lock = r.lock_actor(&owner.id).unwrap();
    let c2 = owner.note("two");
    let c3 = owner.note("three");
    r.append(&lock, &[c2.clone(), c3.clone()]).unwrap();
    assert_eq!(log_of(&r, &owner.id, 2), vec![c2, c3.clone()]);

    // A valid claim followed by one that skips a seq: the batch is refused whole.
    let c4 = owner.note("four");
    let _c5 = owner.note("five");
    let c6 = owner.note("six");
    assert_eq!(r.append(&lock, &[c4.clone(), c6]).unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(r.head(&lock).unwrap().unwrap().seq, 3);

    // A forged claim fails verification; again nothing is written.
    let mut forged = c4.clone();
    forged.body.note = Some("forged".into());
    assert_eq!(r.append(&lock, &[forged]).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(r.head(&lock).unwrap().unwrap().seq, 3);

    // A lock covers its own actor's claims only.
    let mut other = Author::new(&owner.space);
    let o1 = other.note("hello");
    let o2 = other.note("again");
    assert_eq!(r.append(&lock, &[c4.clone(), o1.clone()]).unwrap_err().code, ErrorCode::Internal);
    assert_eq!(r.head(&lock).unwrap().unwrap().seq, 3);

    // A new actor's first claim must be seq 1.
    let other_lock = r.lock_actor(&other.id).unwrap();
    assert_eq!(r.head(&other_lock).unwrap(), None);
    assert_eq!(r.append(&other_lock, std::slice::from_ref(&o2)).unwrap_err().code, ErrorCode::Conflict);
    r.append(&lock, &[c4]).unwrap();
    r.append(&other_lock, &[o1, o2]).unwrap();
    assert_eq!(r.head(&lock).unwrap().unwrap().seq, 4);
    assert_eq!(r.head(&other_lock).unwrap().unwrap().seq, 2);
    let mut actors = vec![owner.id.clone(), other.id.clone()];
    actors.sort();
    assert_eq!(r.actors().unwrap(), actors);
    r.verify().unwrap();
}

#[test]
fn merge_tolerates_gaps_fills_them_and_appends_after() {
    let sa = Scratch::new("merge-src");
    let (a, mut owner, _) = new_space(&sa.0);
    let more: Vec<Claim> = (2..=6).map(|i| owner.note(&format!("n{i}"))).collect();
    a.append(&a.lock_actor(&owner.id).unwrap(), &more).unwrap();
    let all = log_of(&a, &owner.id, 1);
    assert_eq!(seqs(&all), [1, 2, 3, 4, 5, 6]);

    let sb = Scratch::new("merge-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    let partial = [all[0].clone(), all[1].clone(), all[4].clone(), all[5].clone()];
    let rep = b.merge(&partial).unwrap();
    assert_eq!((rep.added, rep.skipped_existing), (4, 0));
    assert_eq!(rep.gaps, vec![(owner.id.clone(), 3, 4)]);
    assert_eq!(seqs(&log_of(&b, &owner.id, 1)), [1, 2, 5, 6]);
    assert_eq!(b.verify().unwrap().gaps, vec![(owner.id.clone(), 3, 4)]);

    // Fill the gap: its run becomes its own segment, and reads stay in seq order.
    let rep = b.merge(&[all[3].clone(), all[2].clone()]).unwrap();
    assert_eq!(rep.added, 2);
    assert!(rep.gaps.is_empty());
    assert_eq!(log_of(&b, &owner.id, 1), all);
    assert_eq!(seqs(&log_of(&b, &owner.id, 4)), [4, 5, 6]);
    assert_eq!(
        segment_files(&b, &owner.id),
        ["000000000001.jsonl", "000000000003.jsonl", "000000000005.jsonl"]
    );

    let rep = b.merge(&all).unwrap();
    assert_eq!((rep.added, rep.skipped_existing), (0, 6));

    // A local append continues from the highest seq.
    let c7 = owner.note("seven");
    b.append(&b.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c7)).unwrap();
    assert_eq!(seqs(&log_of(&b, &owner.id, 6)), [6, 7]);
    let report = b.verify().unwrap();
    assert_eq!(report.claims, 7);
    assert!(report.gaps.is_empty());
}

#[test]
fn merge_rejects_equivocation() {
    let sa = Scratch::new("equiv-src");
    let (a, mut owner, genesis) = new_space(&sa.0);
    let c2 = owner.note("honest");
    a.append(&a.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c2)).unwrap();
    let fork = owner.seal_at(2, Some(genesis.id.clone()), note("fork"));
    assert_ne!(fork.id, c2.id);

    let sb = Scratch::new("equiv-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    b.merge(&[genesis.clone(), c2.clone()]).unwrap();
    assert_eq!(b.merge(std::slice::from_ref(&fork)).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(log_of(&b, &owner.id, 1), vec![genesis.clone(), c2.clone()]);

    // Both branches in one batch: refused before anything is written.
    let sc = Scratch::new("equiv-batch");
    let c = Replica::create_replica(&sc.0, a.manifest()).unwrap();
    assert_eq!(c.merge(&[genesis, c2, fork]).unwrap_err().code, ErrorCode::Integrity);
    assert!(log_of(&c, &owner.id, 1).is_empty());
}

#[test]
fn merge_rejects_claims_that_do_not_link() {
    let sa = Scratch::new("link-src");
    let (a, mut owner, genesis) = new_space(&sa.0);
    let c2 = owner.note("two");
    let c3 = owner.note("three");
    a.append(&a.lock_actor(&owner.id).unwrap(), &[c2.clone(), c3.clone()]).unwrap();

    let sb = Scratch::new("link-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    b.merge(&[genesis.clone(), c3.clone()]).unwrap();

    // A different seq-2 claim chains onto genesis but not into the existing seq 3.
    let impostor = owner.seal_at(2, Some(genesis.id.clone()), note("impostor"));
    assert_eq!(b.merge(&[impostor]).unwrap_err().code, ErrorCode::Integrity);
    // A seq-4 claim whose prev is not seq 3.
    let stray = owner.seal_at(4, Some(genesis.id.clone()), note("stray"));
    assert_eq!(b.merge(&[stray]).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(seqs(&log_of(&b, &owner.id, 1)), [1, 3]);

    // The genuine claim fills the gap.
    assert_eq!(b.merge(&[c2]).unwrap().added, 1);
    b.verify().unwrap();
}

#[test]
fn merge_rejects_foreign_and_forged_claims() {
    let sa = Scratch::new("foreign");
    let (a, mut owner, _) = new_space(&sa.0);
    let mut elsewhere = Author::new(&Id::new_space().unwrap());
    let foreign = elsewhere.note("other space");
    assert_eq!(a.merge(&[foreign]).unwrap_err().code, ErrorCode::Integrity);
    let mut forged = owner.note("real");
    forged.body.note = Some("edited after signing".into());
    assert_eq!(a.merge(&[forged]).unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(a.generation().unwrap(), None, "a refused merge writes nothing");
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
    assert_eq!(r.read_blob(&id).unwrap(), data);
    assert_eq!(r.blob_len(&id).unwrap(), data.len() as u64);
    let mut streamed = Vec::new();
    std::io::Read::read_to_end(&mut r.open_blob(&id).unwrap(), &mut streamed).unwrap();
    assert_eq!(streamed, data);
    assert_eq!(fs::read(r.blob_file(&id).unwrap()).unwrap(), data);
    assert!(r.has_blob(&Id::parse(&format!("{id}#page=1")).unwrap()).unwrap());
    assert_eq!(r.has_blob(&Id::new_entity().unwrap()).unwrap_err().code, ErrorCode::Invalid);

    // Larger than one copy buffer.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (big_id, _, big_size) = r.put_blob(&mut big.as_slice()).unwrap();
    assert_eq!((big_id.clone(), big_size), (Id::blob_of(&big), big.len() as u64));

    let mut ids = vec![id, big_id];
    ids.sort();
    assert_eq!(r.blob_ids().unwrap(), ids);
    assert_no_temp_files(&r);

    let absent = Id::blob_of(b"never stored");
    assert!(!r.has_blob(&absent).unwrap());
    for err in [r.blob_len(&absent).unwrap_err(), r.read_blob(&absent).unwrap_err(), r.blob_file(&absent).unwrap_err()] {
        assert_eq!(err.code, ErrorCode::NotFound);
    }
    assert_eq!(r.open_blob(&absent).err().unwrap().code, ErrorCode::NotFound);
}

/// Every put writes its temp file directly under `blobs/`, dot-prefixed.
fn assert_no_temp_files(r: &Replica) {
    let leftovers: Vec<_> = fs::read_dir(r.root().join("blobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn put_blob_file_stores_what_put_blob_stores() {
    let s = Scratch::new("blobfile");
    let (by_stream, _, _) = new_space(&s.path("stream"));
    let (by_file, _, _) = new_space(&s.path("file"));
    // Larger than one copy buffer.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    let src = s.path("source.bin");
    fs::write(&src, &data).unwrap();

    let streamed = by_stream.put_blob(&mut data.as_slice()).unwrap();
    let filed = by_file.put_blob_file(&src).unwrap();
    assert_eq!(filed, streamed);
    assert_eq!(filed, (Id::blob_of(&data), true, data.len() as u64));
    assert_eq!(by_file.read_blob(&filed.0).unwrap(), data);
    assert_eq!(fs::read(&src).unwrap(), data, "the source is left as it was");
    assert_eq!(by_file.verify().unwrap().blobs, 1);
    assert_no_temp_files(&by_file);
}

#[cfg(unix)]
#[test]
fn put_blob_file_leaves_a_present_blob_untouched() {
    use std::os::unix::fs::MetadataExt;
    let s = Scratch::new("blobfile-dup");
    let (r, _, _) = new_space(&s.path("r"));
    let src = s.path("source.txt");
    fs::write(&src, b"same bytes twice").unwrap();
    let (id, created, size) = r.put_blob_file(&src).unwrap();
    assert!(created);
    let stored = r.blob_file(&id).unwrap();
    let before = fs::metadata(&stored).unwrap();

    assert_eq!(r.put_blob_file(&src).unwrap(), (id.clone(), false, size));
    let after = fs::metadata(&stored).unwrap();
    // A rename over it would give the path a new inode.
    assert_eq!((after.ino(), after.modified().unwrap()), (before.ino(), before.modified().unwrap()));
    assert_eq!(fs::read(&stored).unwrap(), b"same bytes twice");
    assert_no_temp_files(&r);
}

#[test]
fn put_blob_file_rejects_missing_and_non_regular_sources() {
    let s = Scratch::new("blobfile-bad");
    let (r, _, _) = new_space(&s.path("r"));
    let missing = s.path("no-such-file");
    let err = r.put_blob_file(&missing).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains(&missing.display().to_string()), "{}", err.message);
    assert_no_temp_files(&r);

    let dir = s.path("a-directory");
    fs::create_dir(&dir).unwrap();
    assert_eq!(r.put_blob_file(&dir).unwrap_err().code, ErrorCode::Invalid);
    assert_no_temp_files(&r);
    assert!(r.blob_ids().unwrap().is_empty());
}

#[test]
fn hash_file_gives_the_stored_id_and_size_without_storing() {
    let s = Scratch::new("hashfile");
    let (r, _, _) = new_space(&s.path("r"));
    // Larger than one copy buffer.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
    let src = s.path("source.bin");
    fs::write(&src, &data).unwrap();

    let (id, size) = hash_file(&src).unwrap();
    assert_eq!(fs::read_dir(r.root().join("blobs")).unwrap().count(), 0, "hashing wrote into blobs/");
    assert!(!r.has_blob(&id).unwrap());
    let (stored, _, stored_size) = r.put_blob(&mut data.as_slice()).unwrap();
    assert_eq!((id, size), (stored, stored_size));
}

#[test]
fn hash_file_rejects_missing_and_non_regular_sources() {
    let s = Scratch::new("hashfile-bad");
    let missing = s.path("no-such-file");
    let err = hash_file(&missing).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains(&missing.display().to_string()), "{}", err.message);

    let dir = s.path("a-directory");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(hash_file(&dir).unwrap_err().code, ErrorCode::Invalid);
}

#[cfg(unix)]
#[test]
fn put_blob_file_gives_a_read_only_source_the_streamed_mode() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("blobfile-mode");
    let (r, _, _) = new_space(&s.path("r"));
    let src = s.path("read-only.txt");
    fs::write(&src, b"read-only source").unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o444)).unwrap();

    let (filed, _, _) = r.put_blob_file(&src).unwrap();
    let (streamed, _, _) = r.put_blob(&mut &b"streamed bytes"[..]).unwrap();
    let mode = |id: &Id| fs::metadata(r.blob_file(id).unwrap()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&filed), mode(&streamed));
    r.verify().unwrap();
}

/// macOS copies BSD file flags onto a clone and an `fcopyfile` copy alike; a locked
/// temp file could then be neither renamed nor deleted.
#[cfg(target_os = "macos")]
#[test]
fn put_blob_file_stores_a_locked_source_without_its_lock() {
    use std::os::macos::fs::MetadataExt;
    use std::process::Command;
    const UF_IMMUTABLE: u32 = 0x2;
    /// Unlocks on drop so `Scratch` can delete the source even after a failed assert.
    struct Locked(PathBuf);
    impl Drop for Locked {
        fn drop(&mut self) {
            let _ = Command::new("chflags").arg("nouchg").arg(&self.0).status();
        }
    }
    let s = Scratch::new("blobfile-locked");
    let (r, _, _) = new_space(&s.path("r"));
    let src = s.path("locked.txt");
    fs::write(&src, b"locked source").unwrap();
    let locked = Locked(src.clone());
    assert!(Command::new("chflags").arg("uchg").arg(&locked.0).status().unwrap().success());

    let (id, created, size) = r.put_blob_file(&src).unwrap();
    assert_eq!((id.clone(), created, size), (Id::blob_of(b"locked source"), true, 13));
    assert_eq!(fs::metadata(r.blob_file(&id).unwrap()).unwrap().st_flags() & UF_IMMUTABLE, 0);
    assert_eq!(r.put_blob_file(&src).unwrap(), (id, false, 13));
    assert_no_temp_files(&r);
    r.verify().unwrap();
}

#[test]
fn verify_reports_tampered_logs_and_blobs() {
    let s = Scratch::new("verify");
    let (r, mut owner, _) = new_space(&s.0);
    r.append(&r.lock_actor(&owner.id).unwrap(), &[owner.note("original text")]).unwrap();
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

    fs::write(r.blob_file(&blob).unwrap(), b"blob bytez").unwrap();
    assert_eq!(r.verify().unwrap_err().code, ErrorCode::Integrity);
}

/// A replica with three claims beyond genesis, one referencing a blob, plus two blobs.
fn populated(dir: &Path) -> (Replica, Author, Vec<Claim>, Vec<Id>) {
    let (r, mut owner, _) = new_space(dir);
    let lock = r.lock_actor(&owner.id).unwrap();
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
        assert_eq!(b.read_blob(id).unwrap(), a.read_blob(id).unwrap());
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
    let (a, owner, _) = new_space(&scratch.path("a"));
    let (b, _, _) = new_space(&scratch.path("b"));
    let mut dirs = std::collections::BTreeSet::new();
    for i in 0..32u32 {
        let (id, _, _) = a.put_blob(&mut i.to_le_bytes().as_slice()).unwrap();
        let p = a.blob_file(&id).unwrap();
        dirs.insert(p.parent().unwrap().file_name().unwrap().to_owned());
        assert_eq!(&id.as_str()[6..8], p.parent().unwrap().file_name().unwrap().to_str().unwrap());
    }
    assert!(dirs.len() > 8, "32 blobs landed in only {} fan-out dirs", dirs.len());
    let lock_a = a.lock_actor(&owner.id).unwrap();
    assert_eq!(b.head(&lock_a).unwrap_err().code, ErrorCode::Internal);
    assert_eq!(b.append(&lock_a, &[]).unwrap_err().code, ErrorCode::Internal);
}

// ---------- audit regressions (DESIGN §5, §7.3) ----------

/// Index catch-up and rebuild read through `read_log`, which used to check only
/// structure: an unsigned line (here alice "granting" bot Owner) was believed.
#[test]
fn read_log_rejects_forged_and_edited_lines() {
    let s = Scratch::new("forged");
    let (r, mut owner, _) = new_space(&s.0);
    let c2 = owner.note("genuine text");
    r.append(&r.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c2)).unwrap();
    let segment = segment_path(&r, &owner.id, 1);

    let pristine = fs::read(&segment).unwrap();

    // Well-formed, chained, with the right id; only the signature is missing.
    let bot = Author::new(&owner.space);
    let grant =
        Draft::new(bot.id.clone(), Term::parse("knowa:role").unwrap(), Value::Term(Term::parse("knowa:Owner").unwrap()));
    let body = ClaimBody::from_draft(grant, owner.space.clone(), owner.id.clone(), 3, Some(c2.id.clone()), Timestamp::now());
    let id = Id::claim_of(&body.canonical_bytes().unwrap());
    let forged = Claim { body, id, sig: "A".repeat(86) };
    append_bytes(&segment, &forged.to_line().unwrap());
    for from in [1, 3] {
        let err = r.read_log(&owner.id, from).unwrap().collect::<Result<Vec<_>, _>>().unwrap_err();
        assert_eq!(err.code, ErrorCode::Integrity, "from seq {from}: {err:?}");
    }
    // Claims below `from_seq` are not yielded, so they are not checked either.
    assert_eq!(r.read_log(&owner.id, 4).unwrap().count(), 0);

    // An edited genuine line: its id no longer matches its content.
    fs::write(&segment, replace_once(&pristine, b"genuine text", b"genuine t3xt")).unwrap();
    let err = r.read_log(&owner.id, 1).unwrap().collect::<Result<Vec<_>, _>>().unwrap_err();
    assert_eq!(err.code, ErrorCode::Integrity);
}

/// A crash part-way through an append left an unterminated last line, and every later
/// read and write failed with `integrity` forever.
#[test]
fn taking_the_actor_lock_truncates_a_torn_tail() {
    let s = Scratch::new("torn");
    let (r, mut owner, _) = new_space(&s.0);
    let c2 = owner.note("two");
    r.append(&r.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c2)).unwrap();
    let segment = segment_path(&r, &owner.id, 1);
    let intact = fs::read(&segment).unwrap();

    // What a crash leaves while appending c3: part of its line and no newline.
    let c3 = owner.note("three");
    let line = c3.to_line().unwrap();
    append_bytes(&segment, &line[..line.len() / 2]);

    let lock = r.lock_actor(&owner.id).unwrap();
    assert_eq!(fs::read(&segment).unwrap(), intact);
    assert_eq!(r.head(&lock).unwrap(), Some(Head { seq: 2, id: c2.id.clone() }));
    r.append(&lock, std::slice::from_ref(&c3)).unwrap();
    assert_eq!(seqs(&log_of(&r, &owner.id, 1)), [1, 2, 3]);
    r.verify().unwrap();
    drop(lock);

    // A segment with no complete line is no crashed append (new segments are published
    // whole), so it is left for readers to report.
    let mut other = Author::new(&owner.space);
    let o1 = other.note("hello");
    let dir = r.root().join("log").join(other.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let partial = &o1.to_line().unwrap()[..10];
    fs::write(dir.join("000000000001.jsonl"), partial).unwrap();
    drop(r.lock_actor(&other.id).unwrap());
    assert_eq!(fs::read(dir.join("000000000001.jsonl")).unwrap(), partial);
    assert_eq!(r.read_log(&other.id, 1).unwrap().next().unwrap().unwrap_err().code, ErrorCode::Integrity);
    assert_eq!(r.verify().unwrap_err().code, ErrorCode::Integrity);
}

/// DESIGN §5.3: under the shared lock an unterminated final line can only be an append
/// that never returned, which the next writer truncates; a reader stops before it
/// instead of failing until a writer comes along.
#[test]
fn a_reader_stops_before_an_unterminated_last_line() {
    let s = Scratch::new("torn-read");
    let (r, mut owner, _) = new_space(&s.0);
    let c2 = owner.note("two");
    r.append(&r.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c2)).unwrap();
    let segment = segment_path(&r, &owner.id, 1);
    let line = owner.note("three").to_line().unwrap();
    append_bytes(&segment, &line[..line.len() / 2]);

    assert_eq!(seqs(&log_of(&r, &owner.id, 1)), [1, 2]);
    assert_eq!(seqs(&log_of(&r, &owner.id, 3)), Vec::<u64>::new());
    // The peek ignores it too.
    assert!(!r.may_have_after(&owner.id, 2).unwrap());
    assert!(r.may_have_after(&owner.id, 1).unwrap());
    // Reading took no exclusive lock, so the torn line is still there for the writer.
    assert!(fs::read(&segment).unwrap().ends_with(&line[..line.len() / 2]));
}

/// DESIGN §5.3: exclusivity is per actor, so writers for different actors never wait
/// for each other.
#[test]
fn two_actors_append_to_one_replica_at_once() {
    let s = Scratch::new("two-writers");
    let (r, owner, _) = new_space(&s.0);
    let other = author_after(&owner.space, &owner.id);
    let (locked_tx, locked_rx) = mpsc::channel();
    let mut go = Vec::new();
    std::thread::scope(|scope| {
        for mut author in [owner, other] {
            let (go_tx, go_rx) = mpsc::channel::<()>();
            go.push(go_tx);
            let (r, locked_tx) = (&r, locked_tx.clone());
            scope.spawn(move || {
                let lock = r.lock_actor(&author.id).unwrap();
                locked_tx.send(author.id.clone()).unwrap();
                go_rx.recv().unwrap();
                for i in 0..20 {
                    let batch = [author.note(&format!("{i}a")), author.note(&format!("{i}b"))];
                    r.append(&lock, &batch).unwrap();
                }
            });
        }
        // Both locks are held at the same time, or one of these never arrives.
        for _ in 0..2 {
            locked_rx.recv_timeout(Duration::from_secs(10)).expect("an actor lock waited for another actor's");
        }
        for tx in &go {
            tx.send(()).unwrap();
        }
    });
    let actors = r.actors().unwrap();
    assert_eq!(actors.len(), 2);
    let owner_seqs = seqs(&log_of(&r, &r.manifest().owner, 1));
    assert_eq!(owner_seqs, (1..=41).collect::<Vec<u64>>());
    let other_id = actors.iter().find(|a| **a != r.manifest().owner).unwrap();
    assert_eq!(seqs(&log_of(&r, other_id, 1)), (1..=40).collect::<Vec<u64>>());
    let report = r.verify().unwrap();
    assert_eq!((report.claims, report.actors), (81, 2));
    assert!(report.gaps.is_empty());
}

/// flock is per open file: a second replica handle (another process, in effect) waits
/// for the lock, and so does its reader of that actor; a reader of another actor does not.
#[test]
fn a_second_lock_on_an_actor_waits_until_the_first_is_dropped() {
    let s = Scratch::new("lock-wait");
    let (r1, mut owner, _) = new_space(&s.0);
    let mut other = Author::new(&owner.space);
    r1.append(&r1.lock_actor(&other.id).unwrap(), &[other.note("hello")]).unwrap();
    let r2 = Replica::open(&s.0).unwrap();

    let held = r1.lock_actor(&owner.id).unwrap();
    assert_eq!(r1.lock_actor(&owner.id).err().unwrap().code, ErrorCode::Internal, "this thread already holds it");
    let c2 = owner.note("appended, lock still held");
    r1.append(&held, std::slice::from_ref(&c2)).unwrap();
    // This thread holds the lock through r1, so r1 reads the actor without taking it again.
    assert_eq!(seqs(&log_of(&r1, &owner.id, 1)), [1, 2]);

    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let (r2, owner_id, other_id) = (&r2, owner.id.clone(), other.id.clone());
        let lock_tx = tx.clone();
        scope.spawn(move || {
            let lock = r2.lock_actor(&owner_id).unwrap();
            lock_tx.send(format!("locked at {}", r2.head(&lock).unwrap().unwrap().seq)).unwrap();
        });
        let read_tx = tx.clone();
        let reader_owner = owner.id.clone();
        scope.spawn(move || {
            let n = r2.read_log(&reader_owner, 1).unwrap().count();
            read_tx.send(format!("read {n}")).unwrap();
        });
        assert_eq!(r2.read_log(&other_id, 1).unwrap().count(), 1, "another actor's log is readable meanwhile");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "a second lock or a reader did not wait");
        drop(held);
        let mut got = vec![rx.recv_timeout(Duration::from_secs(10)).unwrap(), rx.recv_timeout(Duration::from_secs(10)).unwrap()];
        got.sort();
        assert_eq!(got, ["locked at 2", "read 2"]);
    });
}

/// DESIGN §5.4: a merge that adds claims writes a fresh generation; one that adds
/// nothing leaves it.
#[test]
fn merge_writes_a_fresh_generation_only_when_it_adds() {
    let sa = Scratch::new("gen-src");
    let (a, mut owner, genesis) = new_space(&sa.0);
    let c2 = owner.note("two");
    a.append(&a.lock_actor(&owner.id).unwrap(), std::slice::from_ref(&c2)).unwrap();
    assert_eq!(a.generation().unwrap(), None, "appends do not touch it");

    let sb = Scratch::new("gen-dst");
    let b = Replica::create_replica(&sb.0, a.manifest()).unwrap();
    assert_eq!(b.generation().unwrap(), None);
    b.merge(std::slice::from_ref(&genesis)).unwrap();
    let first = b.generation().unwrap().unwrap();
    assert_eq!(fs::read_to_string(sb.path("generation")).unwrap(), format!("{first}\n"));

    assert_eq!(b.merge(std::slice::from_ref(&genesis)).unwrap().skipped_existing, 1);
    assert_eq!(b.generation().unwrap().as_ref(), Some(&first));

    b.merge(std::slice::from_ref(&c2)).unwrap();
    assert_ne!(b.generation().unwrap().unwrap(), first);

    fs::write(sb.path("generation"), b"not a ulid\n").unwrap();
    assert_eq!(b.generation().unwrap_err().code, ErrorCode::Integrity);
}

#[test]
fn import_writes_a_fresh_generation_when_it_adds_claims_or_blobs() {
    let s = Scratch::new("import-gen");
    let (a, owner, claims, blobs) = populated(&s.path("a"));
    write_bundle(&a, &s.path("claims.knowa"), &meta_for(&a, "shared", &owner.id), &claims, &[]).unwrap();
    write_bundle(&a, &s.path("all.knowa"), &meta_for(&a, "private", &owner.id), &claims, &blobs).unwrap();

    let (b, _, _) = import_bundle(&s.path("claims.knowa"), &s.path("b")).unwrap();
    let first = b.generation().unwrap().unwrap();

    let (b, _, rep) = import_bundle(&s.path("claims.knowa"), &s.path("b")).unwrap();
    assert_eq!(rep.added, 0);
    assert_eq!(b.generation().unwrap().as_ref(), Some(&first));

    // Only blobs are new: a text blob changes the full-text documents of claims that
    // are already indexed, which catch-up never revisits.
    let (b, _, rep) = import_bundle(&s.path("all.knowa"), &s.path("b")).unwrap();
    assert_eq!((rep.added, rep.blobs_added), (0, 2));
    assert_ne!(b.generation().unwrap().unwrap(), first);
    assert!(!s.path("b").join(".index").exists(), "the replica holds nothing machine-local");
}

/// DESIGN §5.2: the key names one location, however the path to it is spelled.
#[cfg(unix)]
#[test]
fn location_key_is_stable_and_differs_per_directory() {
    let s = Scratch::new("location-key");
    let (a, _, _) = new_space(&s.path("a"));
    let (b, _, _) = new_space(&s.path("b"));
    let key = a.location_key().unwrap();
    assert_eq!(key.len(), 16);
    assert!(key.bytes().all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c)), "{key}");
    assert_eq!(Replica::open(&s.path("a")).unwrap().location_key().unwrap(), key);
    std::os::unix::fs::symlink(s.path("a"), s.path("link")).unwrap();
    assert_eq!(Replica::open(&s.path("link")).unwrap().location_key().unwrap(), key);
    assert_eq!(Replica::open(&s.path("b/../a")).unwrap().location_key().unwrap(), key);
    assert_ne!(b.location_key().unwrap(), key);
}

/// DESIGN §7.3: the manifest's owner and genesis must be backed by a real genesis
/// claim, or a bundle can name anyone as the owner of a space.
#[test]
fn import_rejects_a_genesis_that_is_not_an_owner_claim() {
    let s = Scratch::new("fake-genesis");
    let space = Id::new_space().unwrap();
    let mut mallory = Author::new(&space);
    let victim = Author::new(&space);
    let not_owner_claims = [
        mallory.note("seq 1, but not a knowa:owner claim"),
        Author::new(&space).push(Draft::new(space.clone(), Term::parse(OWNER).unwrap(), Value::Ref(victim.id.clone()))),
    ];
    for (i, first) in not_owner_claims.into_iter().enumerate() {
        let manifest =
            Manifest { format: 1, space: space.clone(), owner: first.body.actor.clone(), genesis: first.id.clone() };
        let m = Replica::create_replica(&s.path(&format!("m{i}")), &manifest).unwrap();
        m.merge(std::slice::from_ref(&first)).unwrap();
        let bundle = s.path(&format!("m{i}.knowa"));
        write_bundle(&m, &bundle, &meta_for(&m, "private", &first.body.actor), std::slice::from_ref(&first), &[]).unwrap();
        let err = import_bundle(&bundle, &s.path(&format!("dest{i}"))).err().unwrap();
        assert_eq!(err.code, ErrorCode::Integrity, "case {i}: {err:?}");
        assert!(!s.path(&format!("dest{i}")).exists(), "case {i}");
    }
}
