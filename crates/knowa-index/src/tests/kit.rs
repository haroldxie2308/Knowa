//! Test fixtures: a signed-claim factory, a temp directory and an in-memory blob
//! store, so index behavior can be tested without a replica directory.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use knowa_model::query::View;
use knowa_model::{Claim, ClaimBody, Draft, Id, Result, Signer, Status, Term, Timestamp, Value, vocab};

use crate::Index;
use crate::blobs::BlobSource;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!(
            "knowa-index-unit-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            nanos
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best-effort cleanup of scratch data; a leftover temp dir fails no test.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
pub struct MemBlobs(pub BTreeMap<Id, Vec<u8>>);

impl MemBlobs {
    pub fn put(&mut self, bytes: &[u8]) -> Id {
        let id = Id::blob_of(bytes);
        self.0.insert(id.clone(), bytes.to_vec());
        id
    }
}

impl BlobSource for MemBlobs {
    fn has_blob(&self, id: &Id) -> bool {
        self.0.contains_key(&id.base())
    }
    fn blob_ids(&self) -> Result<Vec<Id>> {
        Ok(self.0.keys().cloned().collect())
    }
    fn blob_len(&self, id: &Id) -> Result<u64> {
        Ok(self.0[&id.base()].len() as u64)
    }
    fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        Ok(self.0[&id.base()].clone())
    }
}

pub fn term(s: &str) -> Value {
    Value::Term(Term::parse(s).unwrap())
}

pub fn text(s: &str) -> Value {
    Value::text(s)
}

pub fn when(s: &str) -> Value {
    Value::When(knowa_model::When::parse(s).unwrap())
}

pub fn geo(lat: &str, lon: &str) -> Value {
    Value::Geo(knowa_model::Geo { lat: knowa_model::Dec::parse(lat).unwrap(), lon: knowa_model::Dec::parse(lon).unwrap(), alt: None, r: None })
}

pub fn all_statuses() -> View {
    View {
        status: vec![Status::Accepted, Status::Proposed, Status::Rejected, Status::Retracted, Status::Superseded],
        ..View::default()
    }
}

/// Actor 0 is the genesis owner.
pub const G: usize = 0;

/// Signs claims for a fixed space and a set of actors, keeping each actor's chain.
pub struct World {
    pub space: Id,
    signers: Vec<Signer>,
    heads: HashMap<usize, (u64, Id)>,
    /// record time of the next claim; advanced by 1 s per claim unless set
    pub clock: i64,
}

impl World {
    pub fn new(actors: usize) -> World {
        World {
            space: Id::new_space().unwrap(),
            signers: (0..actors).map(|i| Signer::from_seed([i as u8 + 1; 32])).collect(),
            heads: HashMap::new(),
            clock: 1_700_000_000_000,
        }
    }

    pub fn actor(&self, who: usize) -> Id {
        self.signers[who].actor_id()
    }

    pub fn owner(&self) -> Id {
        self.actor(G)
    }

    pub fn draft(&mut self, who: usize, d: Draft) -> Claim {
        let actor = self.actor(who);
        let (seq, prev) = match self.heads.get(&who) {
            Some((seq, id)) => (seq + 1, Some(id.clone())),
            None => (1, None),
        };
        let body = ClaimBody::from_draft(d, self.space.clone(), actor, seq, prev, Timestamp(self.clock));
        self.clock += 1000;
        let c = self.signers[who].seal(body).unwrap();
        self.heads.insert(who, (seq, c.id.clone()));
        c
    }

    pub fn say(&mut self, who: usize, s: &Id, p: &str, o: Value) -> Claim {
        self.draft(who, Draft::new(s.clone(), Term::parse(p).unwrap(), o))
    }

    /// The genesis claim: must be the owner's first claim.
    pub fn genesis(&mut self) -> Claim {
        let (space, owner) = (self.space.clone(), self.owner());
        self.say(G, &space, vocab::OWNER, Value::Ref(owner))
    }

    pub fn kind(&mut self, who: usize, kind: &str) -> Claim {
        let me = self.actor(who);
        self.say(who, &me, vocab::ACTOR_KIND, term(kind))
    }

    pub fn grant(&mut self, who: usize, to: usize, role: &str) -> Claim {
        let to = self.actor(to);
        self.say(who, &to, vocab::ROLE, term(role))
    }

    pub fn auto_accept(&mut self, who: usize, to: usize, on: bool) -> Claim {
        let to = self.actor(to);
        self.say(who, &to, vocab::AUTO_ACCEPT, Value::Bool(on))
    }

    pub fn review(&mut self, who: usize, target: &Claim, accept: bool) -> Claim {
        let verdict = if accept { vocab::REVIEW_ACCEPT } else { vocab::REVIEW_REJECT };
        self.say(who, &target.id, vocab::REVIEW, term(verdict))
    }

    pub fn retract(&mut self, who: usize, target: &Claim) -> Claim {
        self.say(who, &target.id, vocab::RETRACT, Value::Bool(true))
    }

    pub fn label(&mut self, who: usize, s: &Id, l: &str) -> Claim {
        self.say(who, s, vocab::LABEL, text(l))
    }

    /// A claim whose draft is adjusted by `f` (src, replaces, conf, …).
    pub fn with(&mut self, who: usize, s: &Id, p: &str, o: Value, f: impl FnOnce(&mut Draft)) -> Claim {
        let mut d = Draft::new(s.clone(), Term::parse(p).unwrap(), o);
        f(&mut d);
        self.draft(who, d)
    }

    pub fn index(&self, dir: &TempDir) -> Index {
        Index::create(&dir.path().join("index.sqlite"), &self.space, &self.owner(), dir.path()).unwrap()
    }
}

pub fn apply(idx: &mut Index, claims: &[&Claim]) {
    apply_blobs(idx, &MemBlobs::default(), claims)
}

pub fn apply_blobs(idx: &mut Index, blobs: &MemBlobs, claims: &[&Claim]) {
    let owned: Vec<Claim> = claims.iter().map(|c| (*c).clone()).collect();
    idx.apply_with(blobs, &owned).unwrap();
}

pub fn status(idx: &Index, c: &Claim) -> Status {
    idx.claims(std::slice::from_ref(&c.id)).unwrap()[0].status
}

pub fn stale(idx: &Index, c: &Claim) -> bool {
    idx.claims(std::slice::from_ref(&c.id)).unwrap()[0].stale
}

pub fn ent() -> Id {
    Id::new_entity().unwrap()
}
