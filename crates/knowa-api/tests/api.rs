//! `Knowa` api behavior that needs a real home and replicas: space identity (DESIGN
//! §7.3), the actorKind prelude (§4.6) and verify under the write lock (§5).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use knowa_api::{Knowa, Sel};
use knowa_config::Home;
use knowa_model::query::QueryDoc;
use knowa_model::vocab::{ACTOR_KIND, OWNER};
use knowa_model::{ActorKind, ClaimBody, Draft, ErrorCode, Id, Signer, Term, Timestamp, Value};
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
