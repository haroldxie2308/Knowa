//! The write retry on an object store (DESIGN §5.3): a `Txn` whose append loses to
//! another writer of the same actor redoes its write on the new head, and gives up
//! with `conflict` after `WRITE_ATTEMPTS` attempts.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream::BoxStream;
use knowa_config::{Actor, Home, SpaceRef};
use knowa_model::{ActorKind, ClaimBody, Draft, ErrorCode, Id, Term, Timestamp, Value, vocab};
use knowa_store::Replica;
use object_store::memory::InMemory;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMode,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

use crate::select::{Space, blob_cache};
use crate::write::{Txn, WRITE_ATTEMPTS};

const URL: &str = "s3://bucket/notes";

type Fut<'a, T> = Pin<Box<dyn Future<Output = object_store::Result<T>> + Send + 'a>>;

/// An in-memory store on which another writer wins append races: each create-only PUT
/// under `under` first creates the next queued rival object at the same key.
#[derive(Debug)]
struct Rivals {
    inner: InMemory,
    under: String,
    rivals: Mutex<VecDeque<Vec<u8>>>,
}

impl fmt::Display for Rivals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rivals({})", self.inner)
    }
}

impl ObjectStore for Rivals {
    fn put_opts<'life0, 'life1, 'async_trait>(
        &'life0 self,
        location: &'life1 Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Fut<'async_trait, PutResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if matches!(opts.mode, PutMode::Create) && location.as_ref().starts_with(&self.under) {
                let rival = self.rivals.lock().unwrap().pop_front();
                if let Some(rival) = rival {
                    self.inner.put_opts(location, PutPayload::from(rival), PutMode::Create.into()).await?;
                }
            }
            self.inner.put_opts(location, payload, opts).await
        })
    }

    fn put_multipart_opts<'life0, 'life1, 'async_trait>(
        &'life0 self,
        location: &'life1 Key,
        opts: PutMultipartOptions,
    ) -> Fut<'async_trait, Box<dyn MultipartUpload>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.put_multipart_opts(location, opts)
    }

    fn get_opts<'life0, 'life1, 'async_trait>(
        &'life0 self,
        location: &'life1 Key,
        options: GetOptions,
    ) -> Fut<'async_trait, GetResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.get_opts(location, options)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Key>>,
    ) -> BoxStream<'static, object_store::Result<Key>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_delimiter<'life0, 'life1, 'async_trait>(
        &'life0 self,
        prefix: Option<&'life1 Key>,
    ) -> Fut<'async_trait, ListResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.list_with_delimiter(prefix)
    }

    fn copy_opts<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        from: &'life1 Key,
        to: &'life2 Key,
        options: CopyOptions,
    ) -> Fut<'async_trait, ()>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.copy_opts(from, to, options)
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Scratch(std::env::temp_dir().join(format!("knowa-api-race-{tag}-{}-{n}", std::process::id())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn label(text: &str) -> Draft {
    Draft::new(Id::new_entity().unwrap(), Term::parse(vocab::LABEL).unwrap(), Value::text(text))
}

/// Alice's space on a `Rivals` store, with one write of hers in it already.
fn alice_on_rivals(s: &Scratch) -> (Home, Actor, Arc<Rivals>, Space) {
    let home = Home::open(Some(&s.0.join("home"))).unwrap();
    let alice = home.new_actor("alice", ActorKind::Human).unwrap();
    let store = Arc::new(Rivals {
        inner: InMemory::new(),
        under: format!("notes/log/{}/", alice.id),
        rivals: Mutex::new(VecDeque::new()),
    });
    let space_id = Id::new_space().unwrap();
    let owner = Draft::new(space_id.clone(), Term::parse(vocab::OWNER).unwrap(), Value::Ref(alice.id.clone()));
    let genesis = alice
        .signer
        .seal(ClaimBody::from_draft(owner, space_id, alice.id.clone(), 1, None, Timestamp::now()))
        .unwrap();
    let replica = Replica::create_on(store.clone(), URL, &blob_cache(&home), &genesis).unwrap();
    let sref = SpaceRef { alias: None, location: URL.into(), blobs: None, readonly: false };
    let space = Space::new(&home, sref, replica).unwrap();
    Txn::begin(&space, &[&alice]).unwrap().write(&alice, vec![label("first")]).unwrap();
    (home, alice, store, space)
}

/// Queues `drafts` as alice's next claims, written by another process: each wins the
/// race for its seq.
fn queue_rivals(home: &Home, store: &Arc<Rivals>, alice: &Actor, drafts: Vec<Draft>) -> Vec<Id> {
    let other = Replica::open_on(store.clone(), URL, &blob_cache(home)).unwrap();
    let head = other.head(&other.lock_actor(&alice.id).unwrap()).unwrap().unwrap();
    let (mut seq, mut prev) = (head.seq, head.id);
    let mut ids = Vec::new();
    for d in drafts {
        seq += 1;
        let body = ClaimBody::from_draft(d, other.manifest().space.clone(), alice.id.clone(), seq, Some(prev), Timestamp::now());
        let claim = alice.signer.seal(body).unwrap();
        store.rivals.lock().unwrap().push_back(claim.to_line().unwrap());
        prev = claim.id.clone();
        ids.push(claim.id);
    }
    ids
}

/// DESIGN §5.3: the loser catches up, redoes its plan (the rival wrote one of the same
/// claims, which it now finds equivalent), rebuilds on the new head and lands; the
/// chain stays intact.
#[test]
fn a_write_that_loses_the_race_is_redone_on_the_new_head() {
    let s = Scratch::new("redo");
    let (home, alice, store, space) = alice_on_rivals(&s);
    let shared = label("shared");
    let rival = queue_rivals(&home, &store, &alice, vec![shared.clone()]);

    let written = Txn::begin(&space, &[&alice]).unwrap().write(&alice, vec![shared, label("mine")]).unwrap();
    assert!(store.rivals.lock().unwrap().is_empty(), "the rival never ran");
    let items = &written.result.items;
    assert_eq!((&items[0].id, items[0].created), (&rival[0], false), "the rival's equivalent claim is reused");
    assert!(items[1].created);

    let log: Vec<_> = space.replica.read_log(&alice.id, 1).unwrap().map(|c| c.unwrap()).collect();
    let seqs: Vec<u64> = log.iter().map(|c| c.body.seq).collect();
    assert_eq!(seqs, (1..=log.len() as u64).collect::<Vec<_>>());
    let (last, before) = (&log[log.len() - 1], &log[log.len() - 2]);
    assert_eq!((&before.id, &last.id), (&rival[0], &items[1].id));
    assert_eq!(last.body.prev.as_ref(), Some(&rival[0]));
    space.replica.verify().unwrap();
}

/// After `WRITE_ATTEMPTS` lost races the write fails with `conflict` and leaves
/// nothing of its own in the log.
#[test]
fn a_write_gives_up_with_conflict_after_its_attempts() {
    let s = Scratch::new("give-up");
    let (home, alice, store, space) = alice_on_rivals(&s);
    let rivals = (0..WRITE_ATTEMPTS).map(|i| label(&format!("rival {i}"))).collect();
    let rival = queue_rivals(&home, &store, &alice, rivals);

    let err = Txn::begin(&space, &[&alice]).unwrap().write(&alice, vec![label("mine")]).err().unwrap();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert!(store.rivals.lock().unwrap().is_empty());
    let log: Vec<Id> = space.replica.read_log(&alice.id, 1).unwrap().map(|c| c.unwrap().id).collect();
    assert_eq!(log[log.len() - rival.len()..], rival[..]);
    space.replica.verify().unwrap();
}
