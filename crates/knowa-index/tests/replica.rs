//! Index paths that need a real replica directory: open (build / catch-up /
//! rebuild on mismatch or a new rebuild generation), rebuild replay order and epoch,
//! the machine-local index directory, catch-up under per-actor locks, text blobs read
//! from the blob store, export blob presence and `blobs_without_worker`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use knowa_index::Index;
use knowa_model::query::{Order, View, Where};
use knowa_model::{Claim, ClaimBody, Draft, Id, Signer, Status, Term, Timestamp, Value, Visibility, vocab};
use knowa_store::Replica;

/// A scratch directory holding a replica (`replica/`) and, apart from it, the index
/// directory a home would give it (`index/`).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("knowa-index-it-{}-{}-{nanos}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        TempDir(p)
    }
    fn replica(&self) -> PathBuf {
        self.0.join("replica")
    }
    fn index(&self) -> PathBuf {
        self.0.join("index")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best-effort cleanup of scratch data; a leftover temp dir fails no test.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Signs chained claims for one space.
struct Writer {
    space: Id,
    signers: Vec<Signer>,
    heads: Vec<Option<(u64, Id)>>,
    clock: i64,
}

impl Writer {
    fn new(actors: usize) -> Writer {
        Writer {
            space: Id::new_space().unwrap(),
            signers: (0..actors).map(|i| Signer::from_seed([i as u8 + 40; 32])).collect(),
            heads: vec![None; actors],
            clock: 1_700_000_000_000,
        }
    }
    fn actor(&self, who: usize) -> Id {
        self.signers[who].actor_id()
    }
    fn draft(&mut self, who: usize, d: Draft) -> Claim {
        let (seq, prev) = match &self.heads[who] {
            Some((seq, id)) => (seq + 1, Some(id.clone())),
            None => (1, None),
        };
        let body = ClaimBody::from_draft(d, self.space.clone(), self.actor(who), seq, prev, Timestamp(self.clock));
        self.clock += 1000;
        let c = self.signers[who].seal(body).unwrap();
        self.heads[who] = Some((seq, c.id.clone()));
        c
    }
    fn say(&mut self, who: usize, s: &Id, p: &str, o: Value) -> Claim {
        self.draft(who, Draft::new(s.clone(), Term::parse(p).unwrap(), o))
    }
    /// A fresh replica whose genesis owner is actor 0.
    fn replica(&mut self, dir: &TempDir) -> Replica {
        let (space, owner) = (self.space.clone(), self.actor(0));
        let genesis = self.say(0, &space, vocab::OWNER, Value::Ref(owner));
        Replica::create(&dir.replica(), &genesis).unwrap()
    }
}

/// Appends each actor's claims under that actor's lock.
fn append(r: &Replica, claims: &[&Claim]) {
    let mut by_actor: BTreeMap<&Id, Vec<Claim>> = BTreeMap::new();
    for c in claims {
        by_actor.entry(&c.body.actor).or_default().push((*c).clone());
    }
    for (actor, owned) in by_actor {
        r.append(&r.lock_actor(actor).unwrap(), &owned).unwrap();
    }
}

fn feed_ids(idx: &Index) -> Vec<Id> {
    idx.feed(0, 1000).unwrap().0.into_iter().map(|c| c.claim.id).collect()
}

fn search(idx: &Index, q: &str) -> Vec<Id> {
    let w = Where { text: Some(q.into()), ..Where::default() };
    idx.find_entities(&w, &View::default(), Order::Relevance, 50, None).unwrap().items.into_iter().map(|e| e.id).collect()
}

fn indexed(idx: &Index, c: &Claim) -> bool {
    !idx.claims(std::slice::from_ref(&c.id)).unwrap().is_empty()
}

/// Regression: every open (so every write and query) re-parsed each actor's newest
/// segment from its first line, making `put` slower with every claim in the space.
/// An index that already holds an actor's newest claim must not read that actor's
/// older lines: garbling one that is already indexed is invisible to open, and
/// `verify` is what reports it.
#[test]
fn a_current_index_opens_without_rereading_the_log() {
    let dir = TempDir::new();
    let mut w = Writer::new(1);
    let r = w.replica(&dir);
    let e = Id::new_entity().unwrap();
    let c2 = w.say(0, &e, vocab::LABEL, Value::text("indexed"));
    append(&r, &[&c2]);
    drop(Index::open(&r, &dir.index()).unwrap());

    let segment = dir.replica().join("log").join(w.actor(0).as_str()).join("000000000001.jsonl");
    let text = std::fs::read_to_string(&segment).unwrap();
    let (genesis_line, rest) = text.split_once('\n').unwrap();
    std::fs::write(&segment, format!("{}\n{rest}", "x".repeat(genesis_line.len()))).unwrap();

    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_eq!(idx.actor_heads().unwrap(), vec![(w.actor(0), 2)]);
    let err = r.verify().unwrap_err();
    assert_eq!(err.code, knowa_model::ErrorCode::Integrity, "verify still reads every line: {}", err.message);
}

/// DESIGN §5.2: the index is machine-local, in the directory its caller gives.
#[test]
fn open_builds_then_catches_up_with_claims_appended_behind_its_back() {
    let dir = TempDir::new();
    let mut w = Writer::new(2);
    let r = w.replica(&dir);
    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_eq!(idx.actor_heads().unwrap(), vec![(w.actor(0), 1)]);
    let epoch = idx.epoch().to_string();
    drop(idx);

    // appended but never applied (a crash between append and apply)
    let e = Id::new_entity().unwrap();
    let c2 = w.say(0, &e, vocab::LABEL, Value::text("owner's"));
    let c3 = w.say(1, &e, vocab::LABEL, Value::text("other's"));
    append(&r, &[&c2]);
    append(&r, &[&c3]);
    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_eq!(idx.epoch(), epoch, "catch-up keeps the epoch");
    let mut heads = vec![(w.actor(0), 2), (w.actor(1), 1)];
    heads.sort();
    assert_eq!(idx.actor_heads().unwrap(), heads);
    let views = idx.claims(&[c2.id.clone(), c3.id.clone()]).unwrap();
    assert_eq!(views.iter().map(|v| v.status).collect::<Vec<_>>(), vec![Status::Accepted, Status::Proposed]);
    assert_eq!(idx.entity(&e, &View::default()).unwrap().unwrap().label.as_deref(), Some("owner's"));
    assert!(dir.index().join("index.sqlite").is_file());
    assert!(!dir.replica().join(".index").exists(), "nothing machine-local goes into the replica");
}

#[test]
fn rebuild_replays_actors_in_id_order_with_a_new_epoch() {
    let dir = TempDir::new();
    let mut w = Writer::new(3);
    let r = w.replica(&dir);
    let mut idx = Index::open(&r, &dir.index()).unwrap();
    let e = Id::new_entity().unwrap();
    let mut written = Vec::new();
    for (who, label) in [(2, "c"), (1, "b"), (0, "a"), (2, "c2"), (1, "b2")] {
        let c = w.say(who, &e, vocab::LABEL, Value::text(label));
        append(&r, &[&c]);
        idx.apply(&r, std::slice::from_ref(&c)).unwrap();
        written.push(c);
    }
    let review = w.say(0, &written[0].id, vocab::REVIEW, Value::Term(Term::parse(vocab::REVIEW_REJECT).unwrap()));
    append(&r, &[&review]);
    idx.apply(&r, std::slice::from_ref(&review)).unwrap();
    let before_feed = feed_ids(&idx);
    let all_ids: Vec<Id> = before_feed.clone();
    let before = idx.claims(&all_ids).unwrap();
    let epoch = idx.epoch().to_string();
    drop(idx);

    let idx = Index::rebuild(&r, &dir.index()).unwrap();
    assert_ne!(idx.epoch(), epoch);
    assert_eq!(idx.claims(&all_ids).unwrap(), before, "same statuses after rebuild");
    let mut expected: Vec<&Claim> = written.iter().chain([&review]).collect();
    let genesis = r.read_log(&w.actor(0), 1).unwrap().next().unwrap().unwrap();
    expected.push(&genesis);
    expected.sort_by(|a, b| (&a.body.actor, a.body.seq).cmp(&(&b.body.actor, b.body.seq)));
    assert_eq!(feed_ids(&idx), expected.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
    assert_ne!(feed_ids(&idx), before_feed, "arrival numbers are reassigned");
}

#[test]
fn schema_mismatch_or_missing_file_rebuilds() {
    let dir = TempDir::new();
    let mut w = Writer::new(1);
    let r = w.replica(&dir);
    let idx = Index::open(&r, &dir.index()).unwrap();
    let epoch = idx.epoch().to_string();
    drop(idx);
    let path = dir.index().join("index.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE meta SET value = '0' WHERE key = 'schema_version'", []).unwrap();
    drop(conn);
    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_ne!(idx.epoch(), epoch, "schema mismatch → rebuild");
    assert_eq!(feed_ids(&idx).len(), 1);
    let epoch = idx.epoch().to_string();
    drop(idx);
    std::fs::remove_file(&path).unwrap();
    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_ne!(idx.epoch(), epoch, "missing file → rebuild");
    assert_eq!(feed_ids(&idx).len(), 1);
}

/// DESIGN §5.4: catch-up cannot see a merged run that fills a gap below an indexed
/// head; the merge's new rebuild generation makes every index rebuild over it.
#[test]
fn a_new_rebuild_generation_forces_a_rebuild() {
    let dir = TempDir::new();
    let mut w = Writer::new(2);
    let r = w.replica(&dir);
    let e = Id::new_entity().unwrap();
    let c1 = w.say(1, &e, vocab::LABEL, Value::text("first"));
    let c2 = w.say(1, &e, vocab::LABEL, Value::text("second"));
    // a partial import: seq 2 without seq 1
    r.merge(std::slice::from_ref(&c2)).unwrap();
    let idx = Index::open(&r, &dir.index()).unwrap();
    let epoch = idx.epoch().to_string();
    assert!(indexed(&idx, &c2));
    assert!(!indexed(&idx, &c1));
    drop(idx);

    let generation = r.generation().unwrap();
    r.merge(std::slice::from_ref(&c1)).unwrap();
    assert_ne!(r.generation().unwrap(), generation);
    let idx = Index::open(&r, &dir.index()).unwrap();
    assert_ne!(idx.epoch(), epoch, "the new generation forces a rebuild");
    assert!(indexed(&idx, &c1), "the gap-filling claim is indexed");
    let epoch = idx.epoch().to_string();
    drop(idx);
    assert_eq!(Index::open(&r, &dir.index()).unwrap().epoch(), epoch, "an unchanged generation: open catches up");
}

/// `knowa reindex` records the generation it was built against, so the next open
/// does not rebuild again.
#[test]
fn an_explicit_rebuild_records_the_generation() {
    let dir = TempDir::new();
    let mut w = Writer::new(2);
    let r = w.replica(&dir);
    drop(Index::open(&r, &dir.index()).unwrap());
    let c1 = w.say(1, &Id::new_entity().unwrap(), vocab::LABEL, Value::text("merged"));
    r.merge(std::slice::from_ref(&c1)).unwrap();
    let idx = Index::rebuild(&r, &dir.index()).unwrap();
    assert!(indexed(&idx, &c1));
    let epoch = idx.epoch().to_string();
    drop(idx);
    assert_eq!(Index::open(&r, &dir.index()).unwrap().epoch(), epoch, "no second, pointless rebuild");
}

/// DESIGN §5.3/§5.4: readers wait only for a writer of an actor they must catch up;
/// writers catch up only their own actors, so they do not wait for each other.
#[test]
fn catch_up_waits_only_for_an_append_past_the_indexed_head() {
    let dir = TempDir::new();
    let mut w = Writer::new(3);
    let r = w.replica(&dir);
    let e = Id::new_entity().unwrap();
    let c1 = w.say(1, &e, vocab::LABEL, Value::text("one"));
    let c2 = w.say(2, &e, vocab::LABEL, Value::text("two"));
    append(&r, &[&c1, &c2]);
    drop(Index::open(&r, &dir.index()).unwrap());
    // Another process, in effect: its locks are separate open files.
    let other = Replica::open(&dir.replica()).unwrap();
    let wait = Duration::from_millis(300);

    // An actor locked, but with nothing past the indexed head: no wait.
    let idle = other.lock_actor(&w.actor(1)).unwrap();
    // What a reader thread saw of c3, once its open returned.
    let (tx, rx) = mpsc::channel::<knowa_model::Result<bool>>();
    let c3 = w.say(2, &e, vocab::LABEL, Value::text("three"));
    let reader = |r: &Replica, index_dir: PathBuf, c3: Claim, tx: mpsc::Sender<knowa_model::Result<bool>>| {
        tx.send(Index::open(r, &index_dir).map(|idx| indexed(&idx, &c3))).unwrap()
    };
    std::thread::scope(|scope| {
        let (r, index_dir, c3, tx) = (&r, dir.index(), c3.clone(), tx.clone());
        scope.spawn(move || reader(r, index_dir, c3, tx));
        let seen = rx.recv_timeout(Duration::from_secs(10)).expect("catch-up waited for an actor it had nothing to read from");
        assert!(!seen.unwrap());
    });
    drop(idle);

    // An append past the head whose writer still holds the lock (it has not applied
    // yet): a reader waits for it, then sees the claims.
    let in_flight = other.lock_actor(&w.actor(2)).unwrap();
    other.append(&in_flight, std::slice::from_ref(&c3)).unwrap();

    // A writer of actor 1 catches up only actor 1, so it does not wait either.
    {
        let mut writer_index = Index::load(&r, &dir.index()).unwrap();
        let mine = r.lock_actor(&w.actor(1)).unwrap();
        writer_index.catch_up_actors(&r, std::slice::from_ref(mine.actor())).unwrap();
        assert!(!indexed(&writer_index, &c3));
    }

    std::thread::scope(|scope| {
        let (r, index_dir, c3, tx) = (&r, dir.index(), c3.clone(), tx.clone());
        scope.spawn(move || reader(r, index_dir, c3, tx));
        assert!(rx.recv_timeout(wait).is_err(), "catch-up read past an append in flight");
        drop(in_flight);
        let seen = rx.recv_timeout(Duration::from_secs(10)).expect("catch-up still waiting after the writer let go");
        assert!(seen.unwrap(), "the reader did not see the claims it waited for");
    });
}

/// Two machines reading one shared replica keep two indexes, one under each home.
#[test]
fn indexes_in_two_directories_follow_one_replica() {
    let dir = TempDir::new();
    let mut w = Writer::new(2);
    let r = w.replica(&dir);
    let (a, b) = (dir.0.join("home-a-index"), dir.0.join("home-b-index"));
    let e = Id::new_entity().unwrap();
    let c1 = w.say(1, &e, vocab::LABEL, Value::text("first"));
    let c2 = w.say(1, &e, vocab::LABEL, Value::text("second"));
    r.merge(std::slice::from_ref(&c2)).unwrap();
    for index_dir in [&a, &b] {
        assert!(indexed(&Index::open(&r, index_dir).unwrap(), &c2));
    }

    // Machine A fills the gap below both indexed heads.
    r.merge(std::slice::from_ref(&c1)).unwrap();
    assert!(indexed(&Index::open(&r, &a).unwrap(), &c1));
    assert!(indexed(&Index::open(&r, &b).unwrap(), &c1), "B sees the filled gap");
}

#[test]
fn text_blobs_are_read_from_the_replica() {
    let dir = TempDir::new();
    let mut w = Writer::new(1);
    let r = w.replica(&dir);
    let mut idx = Index::open(&r, &dir.index()).unwrap();
    let (txt, _, _) = r.put_blob(&mut "searchable 東京タワー words".as_bytes()).unwrap();
    let mut big = vec![b'x'; knowa_index::MAX_FTS_BLOB_BYTES as usize];
    big.extend_from_slice(b" toolarge");
    let (huge, _, _) = r.put_blob(&mut big.as_slice()).unwrap();
    let note = Id::new_entity().unwrap();
    let claims = [
        w.say(0, &txt, vocab::MEDIA_TYPE, Value::text("text/plain")),
        w.say(0, &huge, vocab::MEDIA_TYPE, Value::text("text/plain")),
        w.say(0, &note, vocab::CONTENT, Value::Ref(txt.clone())),
    ];
    append(&r, &claims.iter().collect::<Vec<_>>());
    idx.apply(&r, &claims).unwrap();
    let mut want = vec![txt.clone(), note.clone()];
    want.sort();
    let mut got = search(&idx, "searchable");
    got.sort();
    assert_eq!(got, want);
    assert_eq!(search(&idx, "タワ").len(), 2);
    assert!(search(&idx, "toolarge").is_empty());
    assert_eq!(idx.media_type(&txt).unwrap().as_deref(), Some("text/plain"));
    drop(idx);
    // the blob text survives a rebuild: it is derived from the blob store
    let idx = Index::rebuild(&r, &dir.index()).unwrap();
    assert_eq!(search(&idx, "searchable").len(), 2);
}

#[test]
fn export_and_extract_selection_consult_the_blob_store() {
    let dir = TempDir::new();
    let mut w = Writer::new(1);
    let r = w.replica(&dir);
    let mut idx = Index::open(&r, &dir.index()).unwrap();
    let (done, _, _) = r.put_blob(&mut &b"already extracted"[..]).unwrap();
    let (todo, _, _) = r.put_blob(&mut &b"not yet"[..]).unwrap();
    let absent = Id::blob_of(b"bytes never stored here");
    let by_worker = w.draft(0, {
        let mut d = Draft::new(done.clone(), Term::parse(vocab::SIZE).unwrap(), Value::Int(17));
        d.via.insert("worker".into(), "media@1".into());
        d
    });
    let mentions_absent = w.say(0, &Id::new_entity().unwrap(), vocab::MENTIONS, Value::Ref(absent));
    let claims = [by_worker, mentions_absent];
    append(&r, &claims.iter().collect::<Vec<_>>());
    idx.apply(&r, &claims).unwrap();

    assert_eq!(idx.blobs_without_worker(&r, "media@1").unwrap(), vec![todo.clone()]);
    let mut all = vec![done.clone(), todo.clone()];
    all.sort();
    assert_eq!(idx.blobs_without_worker(&r, "exif@1").unwrap(), all);

    let (claims_out, blobs_out) = idx.export_set(&r, Visibility::Private).unwrap();
    assert_eq!(claims_out.len(), 3, "private exports every claim");
    assert_eq!(blobs_out, all, "private: every present blob (DESIGN §7.3: everything); absent ones cannot ship");
    let (shared, shared_blobs) = idx.export_set(&r, Visibility::Shared).unwrap();
    assert_eq!(shared.len(), 1, "only the genesis policy claim");
    assert!(shared_blobs.is_empty());
}
