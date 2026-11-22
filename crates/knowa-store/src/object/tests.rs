//! The object-store backend against `object_store`'s in-memory store: layout, racing
//! appends, merges and the generations, damaged segment objects, blobs through the
//! cache, split blob locations, corrupted downloads, and the S3 client's required
//! region.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use knowa_model::vocab::OWNER;
use knowa_model::{Claim, ClaimBody, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value};
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::memory::InMemory;
use object_store::path::Path as Key;
use object_store::{ClientConfigKey, ObjectStore, ObjectStoreExt, PutPayload};

use super::{PART_SIZE, Remote, block_on, s3_store, with_default_timeout};
use crate::backend::Logs;
use crate::dir::{DirBlobs, DirLogs};
use crate::object::{ObjectBlobs, ObjectLogs};
use crate::{Location, Replica, Stores, blob};

const URL: &str = "s3://bucket/spaces/notes";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Scratch(std::env::temp_dir().join(format!("knowa-store-object-{tag}-{}-{n}", std::process::id())))
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

    fn at(&self, seq: u64, prev: Option<Id>, text: &str) -> Claim {
        let d = Draft::new(Id::new_entity().unwrap(), Term::parse("test:note").unwrap(), Value::text(text));
        let body = ClaimBody::from_draft(d, self.space.clone(), self.id.clone(), seq, prev, Timestamp::now());
        self.signer.seal(body).unwrap()
    }

    fn push(&mut self, c: Claim) -> Claim {
        self.last = Some((c.body.seq, c.id.clone()));
        c
    }

    fn genesis(&mut self) -> Claim {
        let d = Draft::new(self.space.clone(), Term::parse(OWNER).unwrap(), Value::Ref(self.id.clone()));
        let body = ClaimBody::from_draft(d, self.space.clone(), self.id.clone(), 1, None, Timestamp::now());
        let c = self.signer.seal(body).unwrap();
        self.push(c)
    }

    fn note(&mut self, text: &str) -> Claim {
        let (seq, prev) = match &self.last {
            Some((s, id)) => (s + 1, Some(id.clone())),
            None => (1, None),
        };
        let c = self.at(seq, prev, text);
        self.push(c)
    }
}

fn memory() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

/// A new space on `store` at `URL`, through its own blob cache.
fn new_space(store: &Arc<dyn ObjectStore>, s: &Scratch) -> (Replica, Author) {
    let mut owner = Author::new(&Id::new_space().unwrap());
    let genesis = owner.genesis();
    (Replica::create_on(Arc::clone(store), URL, &s.path("cache-a"), &genesis).unwrap(), owner)
}

fn keys(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    let mut keys: Vec<String> = block_on(futures::StreamExt::collect::<Vec<_>>(store.list(None)))
        .unwrap()
        .into_iter()
        .map(|m| m.unwrap().location.to_string())
        .collect();
    keys.sort();
    keys
}

fn object(store: &Arc<dyn ObjectStore>, key: &str) -> Option<Vec<u8>> {
    match block_on(async { store.get(&Key::from(key)).await?.bytes().await }).unwrap() {
        Ok(b) => Some(b.to_vec()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(e) => panic!("{e}"),
    }
}

fn append(r: &Replica, claims: &[Claim]) -> knowa_model::Result<()> {
    r.append(&r.lock_actor(&claims[0].body.actor).unwrap(), claims)
}

fn seqs(r: &Replica, actor: &Id) -> Vec<u64> {
    r.read_log(actor, 1).unwrap().map(|c| c.unwrap().body.seq).collect()
}

/// DESIGN §5.1: the directory layout, as keys under the URL's prefix. One batch is one
/// segment object named by its first seq; a second handle sees what the first wrote.
#[test]
fn two_handles_create_write_read_and_reopen_one_replica() {
    let s = Scratch::new("handles");
    let store = memory();
    let (a, mut owner) = new_space(&store, &s);
    let batch = [owner.note("one"), owner.note("two")];
    append(&a, &batch).unwrap();

    let b = Replica::open_on(Arc::clone(&store), &format!("{URL}/"), &s.path("cache-b")).unwrap();
    assert_eq!(b.manifest(), a.manifest());
    assert_eq!(b.location(), &Location::Object(URL.into()));
    assert_eq!(b.location_key().unwrap(), a.location_key().unwrap());
    assert_eq!(seqs(&b, &owner.id), [1, 2, 3]);
    append(&b, &[owner.note("three")]).unwrap();
    assert_eq!(seqs(&a, &owner.id), [1, 2, 3, 4]);
    assert_eq!(a.head(&a.lock_actor(&owner.id).unwrap()).unwrap().unwrap().seq, 4);
    assert_eq!(a.actors().unwrap(), [owner.id.clone()]);
    assert!(!a.may_have_after(&owner.id, 4).unwrap());
    assert!(a.may_have_after(&owner.id, 3).unwrap());
    assert!(a.writers_can_race());

    let log = format!("spaces/notes/log/{}", owner.id);
    assert_eq!(
        keys(&store),
        [
            "spaces/notes/knowa.json".to_string(),
            format!("{log}/000000000001.jsonl"),
            format!("{log}/000000000002.jsonl"),
            format!("{log}/000000000004.jsonl"),
        ]
    );
    let batch_object = object(&store, &format!("{log}/000000000002.jsonl")).unwrap();
    assert_eq!(batch_object, [batch[0].to_line().unwrap(), batch[1].to_line().unwrap()].concat());
    assert_eq!(b.verify().unwrap().claims, 4);
}

#[test]
fn create_needs_an_empty_location_and_open_a_manifest() {
    let s = Scratch::new("empty");
    let store = memory();
    let err = Replica::open_on(Arc::clone(&store), URL, &s.path("cache")).err().unwrap();
    assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");

    block_on(store.put(&Key::from("spaces/notes/stray"), PutPayload::from_static(b"x"))).unwrap().unwrap();
    let mut owner = Author::new(&Id::new_space().unwrap());
    let err = Replica::create_on(Arc::clone(&store), URL, &s.path("cache"), &owner.genesis()).err().unwrap();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert_eq!(keys(&store), ["spaces/notes/stray"], "a refused create wrote");
}

/// DESIGN §5.3: two writers that read the same head both try to create the segment
/// object of the next seq; exactly one succeeds, the other gets `conflict`, and the
/// log holds only the winner's batch.
#[test]
fn racing_appends_for_one_actor_conflict_exactly_once() {
    let s = Scratch::new("race");
    let store = memory();
    let (first, mut owner) = new_space(&store, &s);
    append(&first, &[owner.note("base")]).unwrap();
    let second = Replica::open_on(Arc::clone(&store), URL, &s.path("cache-b")).unwrap();

    let head = owner.last.clone().unwrap();
    let rivals = [owner.at(head.0 + 1, Some(head.1.clone()), "mine"), owner.at(head.0 + 1, Some(head.1), "yours")];
    let barrier = Barrier::new(2);
    let results: Vec<knowa_model::Result<()>> = std::thread::scope(|scope| {
        let runs: Vec<_> = [&first, &second]
            .into_iter()
            .zip(&rivals)
            .map(|(r, claim)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let lock = r.lock_actor(&claim.body.actor).unwrap();
                    assert_eq!(r.head(&lock).unwrap().unwrap().seq, 2);
                    barrier.wait();
                    r.append(&lock, std::slice::from_ref(claim))
                })
            })
            .collect();
        runs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let lost: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(lost.len(), 1, "{results:?}");
    assert_eq!(lost[0].code, ErrorCode::Conflict, "{:?}", lost[0]);
    let winner = rivals[results.iter().position(Result::is_ok).unwrap()].id.clone();
    let log: Vec<Id> = first.read_log(&owner.id, 1).unwrap().map(|c| c.unwrap().id).collect();
    assert_eq!(log.len(), 3);
    assert_eq!(log[2], winner);
    first.verify().unwrap();
}

/// A merge writes each run as one segment object, never rotating. Runs above every
/// head write no generation; one that fills a gap overwrites the `generation` and
/// `gaps` objects (DESIGN §5.4). A run whose object already exists is `integrity`.
#[test]
fn merge_writes_runs_as_objects_and_the_generations_only_when_it_fills_a_gap() {
    let s = Scratch::new("merge");
    let store = memory();
    let (r, _) = new_space(&store, &s);
    let mut other = Author::new(&r.manifest().space);
    let c: Vec<Claim> = (0..5).map(|i| other.note(&format!("n{i}"))).collect();
    let generations = || (r.generation().unwrap(), r.gaps().unwrap());

    let report = r.merge(&[c[0].clone(), c[1].clone(), c[4].clone()]).unwrap();
    assert_eq!((report.added, report.gaps.len()), (3, 1));
    let log = format!("spaces/notes/log/{}", other.id);
    assert!(object(&store, &format!("{log}/000000000001.jsonl")).is_some());
    assert!(object(&store, &format!("{log}/000000000005.jsonl")).is_some());
    assert_eq!(generations(), (None, None), "every claim was above its actor's head");
    assert!(object(&store, "spaces/notes/generation").is_none() && object(&store, "spaces/notes/gaps").is_none());

    r.merge(&[c[2].clone(), c[3].clone()]).unwrap();
    let (generation, gaps) = generations();
    let (generation, gaps) = (generation.unwrap(), gaps.unwrap());
    assert_ne!(generation, gaps, "two fresh values");
    assert_eq!(object(&store, "spaces/notes/generation").unwrap(), format!("{generation}\n").as_bytes());
    assert_eq!(object(&store, "spaces/notes/gaps").unwrap(), format!("{gaps}\n").as_bytes());
    assert_eq!(seqs(&r, &other.id), [1, 2, 3, 4, 5]);
    assert_eq!(r.merge(&c).unwrap().skipped_existing, 5);
    assert_eq!(generations(), (Some(generation), Some(gaps)), "a merge that adds nothing writes nothing");
    assert_eq!(r.verify().unwrap().gaps, []);

    // A run whose object another writer created meanwhile is not overwritten.
    let err = r.logs().write_run(&other.id, 3, &[c[2].to_line().unwrap()]).unwrap_err();
    assert_eq!(err.code, ErrorCode::Integrity, "{err:?}");
}

/// DESIGN §5.3: a segment object is written whole, so an unterminated last line is
/// damage, not an append in progress: reading it is `integrity`, naming the segment,
/// and the catch-up peek defers to that read instead of answering from the line
/// before. (A directory stops before such a line instead:
/// `a_reader_stops_before_an_unterminated_last_line` in tests/store.rs.)
#[test]
fn an_unterminated_last_line_of_a_segment_object_is_integrity() {
    let s = Scratch::new("torn-object");
    let store = memory();
    let (r, mut owner) = new_space(&store, &s);
    let batch = [owner.note("two"), owner.note("three")];
    append(&r, &batch).unwrap();
    let key = format!("spaces/notes/log/{}/000000000002.jsonl", owner.id);
    let torn = batch[1].to_line().unwrap();
    let damaged = [batch[0].to_line().unwrap(), torn[..torn.len() / 2].to_vec()].concat();
    block_on(store.put(&Key::from(key.as_str()), PutPayload::from(damaged))).unwrap().unwrap();

    for from in [1, 3] {
        let read: Vec<_> = r.read_log(&owner.id, from).unwrap().collect();
        let err = read.last().unwrap().as_ref().unwrap_err();
        assert_eq!(err.code, ErrorCode::Integrity, "from seq {from}: {err:?}");
        assert!(err.message.contains(&format!("s3://bucket/{key}")), "{}", err.message);
        assert!(err.message.contains("unterminated final line"), "{}", err.message);
    }
    // The last complete line is seq 2, but the peek must not answer from it.
    assert!(r.may_have_after(&owner.id, 2).unwrap());
    assert_eq!(r.verify().unwrap_err().code, ErrorCode::Integrity);
}

/// DESIGN §5.3 Blob path: a put stages the bytes into the cache, named by their id,
/// then uploads them once; a reader on another machine (another cache) downloads and
/// checks them; the logs and the blobs may live at different kinds of location.
#[test]
fn blobs_at_a_separate_object_location_go_through_the_cache() {
    let s = Scratch::new("split");
    let store = memory();
    let blob_url = "s3://bucket/bulk";
    let remote = Remote::new(Arc::clone(&store), blob_url).unwrap();
    let stores = Stores {
        location: Location::Dir(s.path("replica")),
        logs: Box::new(DirLogs::new(&s.path("replica"))),
        blobs: Box::new(ObjectBlobs::new(remote.clone(), &s.path("cache-a"))),
    };
    let mut owner = Author::new(&Id::new_space().unwrap());
    let genesis = owner.genesis();
    let r = Replica::create_in(stores, crate::genesis_manifest(&genesis).unwrap(), &genesis).unwrap();
    let (id, created, size) = r.put_blob(&mut &b"hello blobs"[..]).unwrap();
    assert!(created);
    assert_eq!(size, 11);
    assert_eq!(r.put_blob(&mut &b"hello blobs"[..]).unwrap(), (id.clone(), false, 11));
    let rel = blob::rel_path(&id).unwrap();
    assert_eq!(object(&store, &format!("bulk/{rel}")).unwrap(), b"hello blobs");
    assert!(blob::path(&s.path("cache-a"), &id).unwrap().is_file(), "the staged copy is named by its id");
    assert!(!s.path("replica").join(&rel).exists(), "the bytes went to the blob location");
    assert_eq!(r.blob_ids().unwrap(), std::slice::from_ref(&id));
    assert_eq!(r.blob_len(&id).unwrap(), 11);

    // Another machine: same replica directory and blob location, an empty cache.
    let elsewhere = Stores {
        location: Location::Dir(s.path("replica")),
        logs: Box::new(DirLogs::new(&s.path("replica"))),
        blobs: Box::new(ObjectBlobs::new(remote, &s.path("cache-b"))),
    };
    let r2 = Replica::open_in(elsewhere).unwrap();
    assert_eq!(r2.read_blob(&id).unwrap(), b"hello blobs");
    assert!(blob::path(&s.path("cache-b"), &id).unwrap().is_file());
    let missing = Id::blob_from_sha256(&[7; 32]);
    assert!(!r2.has_blob(&missing).unwrap());
    assert_eq!(r2.read_blob(&missing).unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(r2.blob_len(&missing).unwrap_err().code, ErrorCode::NotFound);
    r2.verify().unwrap();
}

/// The reverse split: logs on the object store, blobs in a directory.
#[test]
fn blobs_in_a_directory_beside_object_logs() {
    let s = Scratch::new("split-reverse");
    let store = memory();
    let stores = Stores {
        location: Location::Object(URL.into()),
        logs: Box::new(ObjectLogs::new(Remote::new(Arc::clone(&store), URL).unwrap())),
        blobs: Box::new(DirBlobs::new(&s.path("bulk"))),
    };
    let mut owner = Author::new(&Id::new_space().unwrap());
    let genesis = owner.genesis();
    let r = Replica::create_in(stores, crate::genesis_manifest(&genesis).unwrap(), &genesis).unwrap();
    let (id, created, _) = r.put_blob(&mut &b"local bytes"[..]).unwrap();
    assert!(created);
    assert!(s.path("bulk").join(blob::rel_path(&id).unwrap()).is_file());
    assert!(keys(&store).iter().all(|k| !k.contains("/blobs/")), "{:?}", keys(&store));
    assert_eq!(r.read_blob(&id).unwrap(), b"local bytes");
}

/// DESIGN §5.3: bytes downloaded into the cache must hash to their id before use. A
/// corrupted object is `integrity`, leaves nothing in the cache, and `verify` reports
/// it from the stored bytes even where the cache holds a good copy.
#[test]
fn a_corrupted_object_is_rejected_on_download() {
    let s = Scratch::new("corrupt");
    let store = memory();
    let (r, _) = new_space(&store, &s);
    let (id, _, _) = r.put_blob(&mut &b"genuine"[..]).unwrap();
    let key = format!("spaces/notes/{}", blob::rel_path(&id).unwrap());
    block_on(store.put(&Key::from(key.as_str()), PutPayload::from_static(b"forged!"))).unwrap().unwrap();

    let fresh = Replica::open_on(Arc::clone(&store), URL, &s.path("cache-b")).unwrap();
    let err = fresh.read_blob(&id).unwrap_err();
    assert_eq!(err.code, ErrorCode::Integrity, "{err:?}");
    assert_eq!(fresh.blob_file(&id).unwrap_err().code, ErrorCode::Integrity);
    let cache: Vec<_> = walk(&s.path("cache-b"));
    assert!(cache.is_empty(), "the cache kept {cache:?}");

    // The writer's own cache still holds the good copy; verify reads the store.
    assert_eq!(r.read_blob(&id).unwrap(), b"genuine");
    let err = r.verify().unwrap_err();
    assert_eq!(err.code, ErrorCode::Integrity);
    assert!(err.message.contains("content hashes to"), "{}", err.message);
}

/// Above `PART_SIZE` a blob is uploaded in parts; the object is the whole file.
#[test]
fn large_blobs_are_uploaded_in_parts() {
    let s = Scratch::new("multipart");
    let store = memory();
    let (r, _) = new_space(&store, &s);
    let bytes: Vec<u8> = (0..PART_SIZE + 1000).map(|i| (i % 251) as u8).collect();
    fs::create_dir_all(&s.0).unwrap();
    let src = s.path("big.bin");
    fs::write(&src, &bytes).unwrap();
    let (id, created, size) = r.put_blob_file(&src).unwrap();
    assert!(created);
    assert_eq!(size, bytes.len() as u64);
    let key = format!("spaces/notes/{}", blob::rel_path(&id).unwrap());
    assert_eq!(object(&store, &key).unwrap(), bytes);
    let fresh = Replica::open_on(Arc::clone(&store), URL, &s.path("cache-b")).unwrap();
    assert_eq!(fresh.read_blob(&id).unwrap(), bytes);
}

/// `last_line` reads a segment's end only, and widens its window for a line longer
/// than the first one.
#[test]
fn the_head_is_found_past_a_line_longer_than_the_tail_window() {
    let s = Scratch::new("long-line");
    let store = memory();
    let (r, mut owner) = new_space(&store, &s);
    let long = "x".repeat(40_000);
    append(&r, &[owner.note("short"), owner.note(&long)]).unwrap();
    let lock = r.lock_actor(&owner.id).unwrap();
    assert_eq!(r.head(&lock).unwrap().unwrap().id, owner.last.clone().unwrap().1);
}

/// DESIGN §5.2: refused credentials are the caller's to fix and name the variables;
/// not found maps as on a directory; anything else is `internal`, naming the key.
#[test]
fn store_errors_map_to_knowa_codes() {
    let remote = Remote::new(memory(), URL).unwrap();
    let key = Key::from("spaces/notes/knowa.json");
    let source = || -> Box<dyn std::error::Error + Send + Sync> { "403".into() };
    for e in [
        object_store::Error::PermissionDenied { path: key.to_string(), source: source() },
        object_store::Error::Unauthenticated { path: key.to_string(), source: source() },
    ] {
        let err = remote.err(&key, e);
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(err.message.contains("AWS_ACCESS_KEY_ID") && err.message.contains("AWS_SECRET_ACCESS_KEY"), "{}", err.message);
    }
    let err = remote.err(&key, object_store::Error::NotFound { path: key.to_string(), source: source() });
    assert_eq!(err.code, ErrorCode::NotFound);
    let err = remote.err(&key, object_store::Error::Generic { store: "S3", source: source() });
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(err.message.contains("s3://bucket/spaces/notes/knowa.json"), "{}", err.message);
    assert!(!err.message.contains("AWS_ACCESS_KEY_ID"), "{}", err.message);
    // Without key variables, the provider chain may be what failed: name the fix.
    let keyless = Remote { keyless: true, ..remote };
    let err = keyless.err(&key, object_store::Error::Generic { store: "S3", source: source() });
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(err.message.contains("AWS_ACCESS_KEY_ID is not set"), "{}", err.message);
}

/// A local HTTP endpoint that answers every request with `status` and an S3 error
/// body: the real S3 client's error paths, without a server. Requests are read up to
/// the end of their headers only, so it serves bodiless requests (GET, LIST).
fn refusing_endpoint(status: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let (mut seen, mut buf) = (Vec::new(), [0u8; 4096]);
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf).unwrap() {
                    0 => break,
                    n => seen.extend_from_slice(&buf[..n]),
                }
            }
            let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>Refused</Code></Error>";
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    format!("http://{addr}")
}

/// DESIGN §5.2: refused credentials are `usage` on every kind of request. The S3
/// client maps the status of a refused GET, but reports a refused LIST (the first
/// request of `init`, and of every catch-up) as `Generic`; it is classified by the
/// status in its message, and a missing bucket (LIST 404) is `not_found`.
#[test]
fn a_refused_list_is_classified_by_its_status() {
    for (status, code) in
        [("403 Forbidden", ErrorCode::Usage), ("401 Unauthorized", ErrorCode::Usage), ("404 Not Found", ErrorCode::NotFound)]
    {
        let builder = AmazonS3Builder::new()
            .with_region("us-east-1")
            .with_endpoint(refusing_endpoint(status))
            .with_allow_http(true)
            .with_access_key_id("key")
            .with_secret_access_key("secret");
        let logs = ObjectLogs::new(Remote::new(s3_store(builder, "bucket").unwrap(), URL).unwrap());
        for (what, err) in [("prepare", logs.prepare().unwrap_err()), ("actors", logs.actors().err().unwrap())] {
            assert_eq!(err.code, code, "{status}, {what}: {err:?}");
        }
        if code == ErrorCode::Usage {
            let err = logs.read_manifest().unwrap_err();
            assert_eq!(err.code, code, "{status}, a GET: {err:?}");
            assert!(err.message.contains("AWS_ACCESS_KEY_ID"), "{}", err.message);
        }
    }
}

/// DESIGN §5.2: requests get five minutes, not `object_store`'s 30 s, so a part of
/// `PART_SIZE` uploads on a slow link; a timeout from `AWS_TIMEOUT` is kept.
#[test]
fn requests_default_to_a_five_minute_timeout() {
    let timeout = |b: &AmazonS3Builder| b.get_config_value(&AmazonS3ConfigKey::Client(ClientConfigKey::Timeout));
    let defaulted = with_default_timeout(AmazonS3Builder::new(), false);
    assert_eq!(timeout(&defaulted).as_deref(), Some("300s"));
    let from_env = AmazonS3Builder::new().with_config(AmazonS3ConfigKey::Client(ClientConfigKey::Timeout), "120s");
    assert_eq!(timeout(&with_default_timeout(from_env, true)).as_deref(), Some("120s"));
}

/// DESIGN §5.2: no silent default region.
#[test]
fn a_missing_region_is_a_usage_error() {
    let err = s3_store(AmazonS3Builder::new(), "bucket").err().unwrap();
    assert_eq!(err.code, ErrorCode::Usage);
    assert!(err.message.contains("AWS_REGION") && err.message.contains("AWS_DEFAULT_REGION"), "{}", err.message);
    s3_store(AmazonS3Builder::new().with_region("eu-west-1"), "bucket").unwrap();
    s3_store(AmazonS3Builder::new().with_config(AmazonS3ConfigKey::DefaultRegion, "eu-west-1"), "bucket").unwrap();
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
