//! `Knowa` api behavior that needs a real home and replicas: space identity (DESIGN
//! §7.3), the actorKind prelude (§4.6) and verify under the write lock (§5).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use knowa_api::{Knowa, PutOptions, Sel};
use knowa_config::Home;
use knowa_model::query::{FindItems, MAX_LIMIT, QueryDoc, QueryResult};
use knowa_model::view::{EntityWhen, WhenSource};
use knowa_model::vocab::{ACTOR_KIND, CREATED, MEDIA_TYPE, MODIFIED, NAME, OWNER, SIZE, SOURCE};
use knowa_model::{ActorKind, ClaimBody, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value, When};
use knowa_store::{BundleMeta, Replica, write_bundle};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("knowa-api-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
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

/// A home whose human actor `alice` owns the current space `main` at `<scratch>/alice-space`.
fn alice(s: &Scratch) -> (Knowa, Sel, Id) {
    let home = Home::open(Some(&s.path("home"))).unwrap();
    home.new_actor("alice", ActorKind::Human).unwrap();
    let k = Knowa::new(home);
    let sel = Sel { space: None, actor: Some("alice".into()) };
    let init = k.init(&sel, &s.path("alice-space"), "alice notes", Some("main")).unwrap();
    (k, sel, init.space)
}

fn term(s: &str) -> Term {
    Term::parse(s).unwrap()
}

/// Mallory's own replica of `space` — a valid genesis, just not alice's — and a bundle of it.
fn impostor(s: &Scratch, space: &Id) -> (PathBuf, PathBuf) {
    let mallory = Signer::generate().unwrap();
    let me = mallory.actor_id();
    let genesis = mallory
        .seal(ClaimBody::from_draft(
            Draft::new(space.clone(), term(OWNER), Value::Ref(me.clone())),
            space.clone(),
            me.clone(),
            1,
            None,
            Timestamp::now(),
        ))
        .unwrap();
    let dir = s.path("mallory-space");
    let r = Replica::create(&dir, &genesis).unwrap();
    let m = r.manifest();
    let meta = BundleMeta {
        format: m.format,
        space: m.space.clone(),
        owner: m.owner.clone(),
        genesis: m.genesis.clone(),
        level: "private".into(),
        partial: false,
        exported_by: me,
        at: Timestamp::now().format(),
    };
    let bundle = s.path("mallory.knowa");
    write_bundle(&r, &bundle, &meta, &[genesis], &[]).unwrap();
    (dir, bundle)
}

fn find_all() -> QueryDoc {
    serde_json::from_value(serde_json::json!({"op": "find", "spaces": ["*"], "where": {"text": "notes"}})).unwrap()
}

#[test]
fn import_and_space_add_refuse_a_second_genesis_for_a_known_space_id() {
    let s = Scratch::new("identity");
    let (k, _, space) = alice(&s);
    let (dir, bundle) = impostor(&s, &space);

    let err = k.import(&bundle, None, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert!(!k.home().root().join("replicas").join(space.as_str()).exists(), "import wrote before refusing");
    let err = k.space_add(&dir, Some("fake"), true).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert!(!k.home().load_config().unwrap().spaces.contains_key("fake"));
}

#[test]
fn queries_refuse_replicas_that_share_a_space_id_but_not_a_genesis() {
    let s = Scratch::new("shadow");
    let (k, sel, space) = alice(&s);
    k.query(&sel, find_all()).unwrap();
    let (dir, _) = impostor(&s, &space);
    // Registered behind the api's back (e.g. by a home set up before the rule existed).
    k.home().register_space("fake", &dir, &space, true).unwrap();

    let err = k.query(&sel, find_all()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    // Selecting only one of them is not ambiguous.
    k.query(&sel, serde_json::from_value(serde_json::json!({"op": "find", "spaces": ["main"]})).unwrap()).unwrap();
}

#[test]
fn explicit_actor_kind_drafts_are_invalid() {
    let s = Scratch::new("actorkind");
    let (k, sel, _) = alice(&s);
    let me = k.home().actor("alice").unwrap().id;
    for kind in ["knowa:Agent", "knowa:Human"] {
        let d = Draft::new(me.clone(), term(ACTOR_KIND), Value::Term(term(kind)));
        let err = k.write(&sel, vec![d]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{kind}: {err:?}");
    }
}

#[test]
fn verify_waits_for_the_write_lock() {
    let s = Scratch::new("verify-lock");
    let (k, sel, _) = alice(&s);
    let replica = Replica::open(&s.path("alice-space")).unwrap();
    let held = replica.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| tx.send(k.verify(&sel)).unwrap());
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "verify ran while another writer held the lock"
        );
        drop(held);
        let report = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
        assert!(report.claims > 0);
    });
}

#[test]
fn a_rejected_actor_kind_is_not_redeclared() {
    use knowa_model::Status;
    use knowa_model::query::{FindDoc, FindItems, QueryResult, Target, View, Where};
    let s = Scratch::new("kind-once");
    let (k, owner, _) = alice(&s);
    let bot_id = k.home().new_actor("bot", ActorKind::Agent).unwrap().id;
    let bot = Sel { space: None, actor: Some("bot".into()) };
    let e = Id::new_entity().unwrap();
    let kind_claims = |k: &Knowa| -> Vec<Id> {
        let doc = QueryDoc::Find(FindDoc {
            spaces: vec![],
            view: View {
                status: vec![Status::Accepted, Status::Proposed, Status::Rejected, Status::Retracted, Status::Superseded],
                ..View::default()
            },
            as_of: None,
            target: Target::Claims,
            where_: Where { s: vec![bot_id.clone()], p: vec![term(ACTOR_KIND)], ..Where::default() },
            order: None,
            limit: None,
            cursor: None,
        });
        match k.query(&owner, doc).unwrap() {
            QueryResult::Find(f) => match f.items {
                FindItems::Claims(c) => c.into_iter().map(|c| c.claim.id).collect(),
                FindItems::Entities(_) => panic!("target is claims"),
            },
            other => panic!("find returned {other:?}"),
        }
    };

    k.write(&bot, vec![Draft::new(e.clone(), term("knowa:label"), Value::text("one"))]).unwrap();
    let first = kind_claims(&k);
    assert_eq!(first.len(), 1, "bot's first write declares its kind");
    // The owner rejects that declaration; bot writes again: no second declaration.
    k.write(&owner, vec![Draft::new(first[0].clone(), term("knowa:review"), Value::Term(term("knowa:Reject")))]).unwrap();
    k.write(&bot, vec![Draft::new(e, term("knowa:label"), Value::text("two"))]).unwrap();
    assert_eq!(kind_claims(&k), first, "only the first actorKind is effective, so none is written after it");
}

// ---------- put: file times (DESIGN §4.6, §4.8, §7) ----------

/// Writes `bytes` to `<scratch>/<rel>` with modification time `mtime`.
fn file_at(s: &Scratch, rel: &str, bytes: &[u8], mtime: SystemTime) -> PathBuf {
    let p = s.path(rel);
    fs::write(&p, bytes).unwrap();
    fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
    p
}

fn find(k: &Knowa, sel: &Sel, doc: serde_json::Value) -> FindItems {
    match k.query(sel, serde_json::from_value(doc).unwrap()).unwrap() {
        QueryResult::Find(f) => f.items,
        other => panic!("find returned {other:?}"),
    }
}

fn entity_when(k: &Knowa, sel: &Sel, id: &Id) -> EntityWhen {
    match find(k, sel, serde_json::json!({"op": "find", "where": {"ids": [id]}})) {
        FindItems::Entities(mut e) if e.len() == 1 => e.remove(0).when.unwrap(),
        other => panic!("expected one entity, got {other:?}"),
    }
}

/// `(value, actor)` of each `p` claim about `s`, oldest first.
fn when_claims(k: &Knowa, sel: &Sel, s: &Id, p: &str) -> Vec<(When, Id)> {
    let doc = serde_json::json!({"op": "find", "target": "claims", "where": {"s": [s], "p": [p]}, "order": "recorded"});
    match find(k, sel, doc) {
        FindItems::Claims(c) => c
            .into_iter()
            .map(|c| match c.claim.body.o {
                Value::When(w) => (w, c.claim.body.actor),
                other => panic!("{p} with a non-when value {other:?}"),
            })
            .collect(),
        FindItems::Entities(_) => panic!("target is claims"),
    }
}

fn claim_count(k: &Knowa, sel: &Sel) -> usize {
    k.feed(sel, None, MAX_LIMIT).unwrap().claims.len()
}

#[test]
fn put_records_the_file_times_and_dates_the_blob_by_its_mtime() {
    let s = Scratch::new("put-mtime");
    let (k, sel, _) = alice(&s);
    let me = k.home().actor("alice").unwrap().id;
    // half a second past a whole second: the claim floors it
    let mtime = UNIX_EPOCH + Duration::from_millis(1_400_000_000_500);
    let path = file_at(&s, "report.txt", b"quarterly numbers\n", mtime);

    let put = k.put(&sel, std::slice::from_ref(&path), PutOptions::default()).unwrap();
    let item = &put.items[0];
    assert!(item.errors.is_empty(), "{:?}", item.errors);
    let modified = when_claims(&k, &sel, &item.blob, MODIFIED);
    assert_eq!(modified.len(), 1);
    assert_eq!((modified[0].0.raw.as_str(), &modified[0].1), ("2014-05-13T16:53:20Z", &me), "written by the putting actor");
    let when = entity_when(&k, &sel, &item.blob);
    assert_eq!((when.source, when.value.as_str()), (WhenSource::File, "2014-05-13T16:53:20Z"));

    if cfg!(target_os = "macos") {
        let birth = fs::metadata(&path).unwrap().created().unwrap();
        let secs = i64::try_from(birth.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap();
        let created = when_claims(&k, &sel, &item.blob, CREATED);
        assert_eq!(created.len(), 1, "macOS reports a birth time");
        assert_eq!((created[0].0.lo, &created[0].1), (Some(secs * 1000), &me));
    }
}

#[test]
fn a_later_copy_of_the_same_bytes_does_not_move_when() {
    let s = Scratch::new("put-copy");
    let (k, sel, _) = alice(&s);
    let bytes = b"the same photo, twice";
    let original = file_at(&s, "original.jpg", bytes, UNIX_EPOCH + Duration::from_secs(1_400_000_000));
    let copy = file_at(&s, "copy.jpg", bytes, UNIX_EPOCH + Duration::from_secs(1_700_000_000));

    let first = k.put(&sel, &[original], PutOptions::default()).unwrap().items.remove(0);
    let second = k.put(&sel, &[copy], PutOptions::default()).unwrap().items.remove(0);
    assert_eq!(first.blob, second.blob);
    let raw: Vec<String> = when_claims(&k, &sel, &first.blob, MODIFIED).into_iter().map(|(w, _)| w.raw).collect();
    assert_eq!(raw, vec!["2014-05-13T16:53:20Z", "2023-11-14T22:13:20Z"], "both observations are kept");
    let when = entity_when(&k, &sel, &first.blob);
    assert_eq!((when.source, when.value.as_str()), (WhenSource::File, "2014-05-13T16:53:20Z"));
}

#[test]
fn re_putting_an_untouched_file_creates_no_claims() {
    let s = Scratch::new("put-again");
    let (k, sel, _) = alice(&s);
    let path = file_at(&s, "notes.txt", b"unchanged\n", UNIX_EPOCH + Duration::from_secs(1_500_000_000));
    let first = k.put(&sel, std::slice::from_ref(&path), PutOptions::default()).unwrap().items.remove(0);
    let before = claim_count(&k, &sel);
    let again = k.put(&sel, &[path], PutOptions::default()).unwrap().items.remove(0);
    assert_eq!(claim_count(&k, &sel), before);
    assert_eq!(again.claims, first.claims, "the existing claims are returned");
}

// ---------- put --no-store: catalog-only blobs (DESIGN §5) ----------

fn catalog_only() -> PutOptions {
    PutOptions { no_store: true, ..PutOptions::default() }
}

/// The objects of the `p` claims about `s`, oldest first.
fn objects(k: &Knowa, sel: &Sel, s: &Id, p: &str) -> Vec<Value> {
    let doc = serde_json::json!({"op": "find", "target": "claims", "where": {"s": [s], "p": [p]}, "order": "recorded"});
    match find(k, sel, doc) {
        FindItems::Claims(c) => c.into_iter().map(|c| c.claim.body.o).collect(),
        FindItems::Entities(_) => panic!("target is claims"),
    }
}

#[test]
fn put_no_store_records_a_file_without_storing_its_bytes() {
    let s = Scratch::new("put-catalog");
    let (k, sel, _) = alice(&s);
    let bytes = b"lecture 3: integrals\n";
    let path = file_at(&s, "lecture.txt", bytes, UNIX_EPOCH + Duration::from_secs(1_400_000_000));

    let item = k.put(&sel, std::slice::from_ref(&path), catalog_only()).unwrap().items.remove(0);
    assert_eq!((&item.blob, item.size), (&Id::blob_of(bytes), bytes.len() as u64));
    assert_eq!((item.created, item.stored), (false, false));
    assert!(item.errors.is_empty(), "{:?}", item.errors);
    assert_eq!(k.blob_path(&sel, &[], &item.blob).unwrap_err().code, ErrorCode::NotFound);

    let b = &item.blob;
    assert_eq!(objects(&k, &sel, b, NAME), vec![Value::text("lecture.txt")]);
    assert_eq!(objects(&k, &sel, b, SOURCE), vec![Value::text(path.to_str().unwrap())]);
    assert_eq!(objects(&k, &sel, b, MODIFIED), vec![Value::When(When::parse("2014-05-13T16:53:20Z").unwrap())]);
    // Extractor output, read from the source.
    assert_eq!(objects(&k, &sel, b, MEDIA_TYPE), vec![Value::text("text/plain")]);
    assert_eq!(objects(&k, &sel, b, SIZE), vec![Value::Int(bytes.len() as i64)]);
    let when = entity_when(&k, &sel, b);
    assert_eq!((when.source, when.value.as_str()), (WhenSource::File, "2014-05-13T16:53:20Z"));

    // Extractors need bytes: both the default run (blobs lacking some extractor's
    // output; this one has no exif@1 claim) and a full re-run skip it.
    for all in [false, true] {
        let r = k.extract(&sel, all).unwrap();
        assert_eq!((r.blobs, r.claims), (0, 0), "all = {all}");
        assert!(r.errors.is_empty(), "all = {all}: {:?}", r.errors);
    }
}

#[test]
fn a_later_put_stores_a_catalog_only_file_under_the_same_id() {
    let s = Scratch::new("put-catalog-then-store");
    let (k, sel, _) = alice(&s);
    let bytes = b"textbook, chapter 1\n";
    let path = file_at(&s, "chapter1.txt", bytes, UNIX_EPOCH + Duration::from_secs(1_500_000_000));
    let catalogued = k.put(&sel, std::slice::from_ref(&path), catalog_only()).unwrap().items.remove(0);
    let before = claim_count(&k, &sel);

    let stored = k.put(&sel, &[path], PutOptions::default()).unwrap().items.remove(0);
    assert_eq!(stored.blob, catalogued.blob);
    assert_eq!((stored.created, stored.stored), (true, true));
    assert_eq!(fs::read(k.blob_path(&sel, &[], &stored.blob).unwrap()).unwrap(), bytes);
    assert_eq!(claim_count(&k, &sel), before, "storing the bytes later writes no claims");
    assert_eq!(stored.claims, catalogued.claims, "the existing claims are returned");
}

#[test]
fn put_no_store_of_already_stored_bytes_reports_them_stored() {
    let s = Scratch::new("put-store-then-catalog");
    let (k, sel, _) = alice(&s);
    let path = file_at(&s, "notes.txt", b"stored first\n", UNIX_EPOCH + Duration::from_secs(1_500_000_000));
    let first = k.put(&sel, std::slice::from_ref(&path), PutOptions::default()).unwrap().items.remove(0);

    let again = k.put(&sel, &[path], catalog_only()).unwrap().items.remove(0);
    assert_eq!(again.blob, first.blob);
    assert_eq!((again.created, again.stored), (false, true));
}

// ---------- bytes stored after the claims about them (DESIGN §5, §7.2) ----------

/// Entities in the current space whose text matches `word`.
fn text_hits(k: &Knowa, sel: &Sel, word: &str) -> Vec<Id> {
    match find(k, sel, serde_json::json!({"op": "find", "where": {"text": word}})) {
        FindItems::Entities(e) => e.into_iter().map(|e| e.id).collect(),
        FindItems::Claims(_) => panic!("target is entities"),
    }
}

/// Regression: every claim already existed, so the put appended none, and the index
/// learnt of the bytes only at the next reindex.
#[test]
fn a_catalog_only_text_file_stored_later_is_searchable_without_a_reindex() {
    let s = Scratch::new("put-catalog-fts");
    let (k, sel, _) = alice(&s);
    let path = file_at(&s, "notes.txt", b"pangolin migration\n", UNIX_EPOCH + Duration::from_secs(1_500_000_000));
    let blob = k.put(&sel, std::slice::from_ref(&path), catalog_only()).unwrap().items.remove(0).blob;
    assert!(text_hits(&k, &sel, "pangolin").is_empty(), "a catalog-only blob has no full-text content");

    k.put(&sel, &[path], PutOptions::default()).unwrap();
    assert_eq!(text_hits(&k, &sel, "pangolin"), vec![blob]);
}

#[test]
fn blob_put_of_bytes_already_described_as_text_is_searchable_without_a_reindex() {
    let s = Scratch::new("blob-put-fts");
    let (k, sel, _) = alice(&s);
    let bytes = b"okapi sightings\n";
    let blob = Id::blob_of(bytes);
    k.write(&sel, vec![Draft::new(blob.clone(), term(MEDIA_TYPE), Value::text("text/plain"))]).unwrap();
    assert!(text_hits(&k, &sel, "okapi").is_empty());

    assert!(k.blob_put(&sel, &mut &bytes[..]).unwrap().created);
    assert_eq!(text_hits(&k, &sel, "okapi"), vec![blob]);
}
