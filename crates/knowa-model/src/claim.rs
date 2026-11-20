//! Claims, drafts and signing (DESIGN §4.4).

use std::collections::BTreeMap;

use crate::error::Result;
use crate::id::Id;
use crate::term::Term;
use crate::time::{Timestamp, When};
use crate::value::{Dec, Value};

pub const MAX_NOTE_BYTES: usize = 4 * 1024;

/// What a writer submits. The system assigns the envelope (`v, space, actor, seq,
/// prev, at`) and seals it. Ids here may be `_:` placeholders (resolved per batch).
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub s: Id,
    pub p: Term,
    pub o: Value,
    pub q: BTreeMap<Term, Vec<Value>>,
    pub valid: Option<When>,
    pub conf: Option<Dec>,
    pub src: Vec<Id>,
    pub replaces: Vec<Id>,
    pub via: BTreeMap<String, String>,
    pub note: Option<String>,
}

impl Draft {
    pub fn new(s: Id, p: Term, o: Value) -> Draft {
        Draft {
            s,
            p,
            o,
            q: BTreeMap::new(),
            valid: None,
            conf: None,
            src: Vec::new(),
            replaces: Vec::new(),
            via: BTreeMap::new(),
            note: None,
        }
    }

    /// Parses the JSONL draft form (same field names/shapes as a claim, envelope
    /// fields absent). Unknown or envelope fields → `invalid`. Empty `q`/`src`/
    /// `replaces`/`via` → `invalid` (omit instead).
    pub fn from_json(v: &serde_json::Value) -> Result<Draft> {
        let _ = v;
        todo!()
    }

    pub fn to_json(&self) -> serde_json::Value {
        todo!()
    }

    /// Field-level validation independent of the envelope: value rules, `conf` ∈ [0,1],
    /// `note` ≤ 4 KiB, `replaces` are claim ids (or placeholders are rejected there),
    /// `via` key grammar, every `q` array non-empty.
    pub fn validate(&self) -> Result<()> {
        todo!()
    }

    /// All placeholder ids (`_:x`) appearing in `s`, `o`, `q` refs and `src`.
    pub fn placeholders(&self) -> Vec<Id> {
        todo!()
    }

    /// Replaces placeholders using `map`; a placeholder missing from `map` → `invalid`.
    pub fn resolve(&self, map: &BTreeMap<Id, Id>) -> Result<Draft> {
        let _ = map;
        todo!()
    }

    /// Idempotency key (DESIGN §4.4): `mh(JCS({s,p,o,q?,valid?}))` rendered as
    /// lowercase b32 without prefix. Placeholders must already be resolved.
    pub fn equiv_key(&self) -> Result<String> {
        todo!()
    }
}

/// A claim without `id`/`sig`. Its JCS serialization is the canonical bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimBody {
    pub v: u32,
    pub space: Id,
    pub actor: Id,
    pub seq: u64,
    pub prev: Option<Id>,
    pub at: Timestamp,
    pub s: Id,
    pub p: Term,
    pub o: Value,
    pub q: BTreeMap<Term, Vec<Value>>,
    pub valid: Option<When>,
    pub conf: Option<Dec>,
    pub src: Vec<Id>,
    pub replaces: Vec<Id>,
    pub via: BTreeMap<String, String>,
    pub note: Option<String>,
}

impl ClaimBody {
    /// Combines a resolved draft with its envelope.
    pub fn from_draft(d: Draft, space: Id, actor: Id, seq: u64, prev: Option<Id>, at: Timestamp) -> ClaimBody {
        ClaimBody {
            v: crate::vocab::CLAIM_VERSION,
            space,
            actor,
            seq,
            prev,
            at,
            s: d.s,
            p: d.p,
            o: d.o,
            q: d.q,
            valid: d.valid,
            conf: d.conf,
            src: d.src,
            replaces: d.replaces,
            via: d.via,
            note: d.note,
        }
    }

    /// Envelope + field validation: kinds (`space` Space, `actor` Actor, `prev` Claim,
    /// `replaces` Claims), `seq ≥ 1`, `prev` present iff `seq > 1`, no placeholders anywhere.
    pub fn validate(&self) -> Result<()> {
        todo!()
    }

    /// JSON object without `id`/`sig`; optional fields omitted when empty/None.
    pub fn to_json(&self) -> serde_json::Value {
        todo!()
    }

    /// JCS bytes of `to_json()`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        todo!()
    }

    /// Same key as `Draft::equiv_key` for the same {s,p,o,q,valid}.
    pub fn equiv_key(&self) -> Result<String> {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub body: ClaimBody,
    pub id: Id,
    /// base64url, no padding
    pub sig: String,
}

impl Claim {
    /// Parses a stored claim (log line / bundle / feed). Structural validation only;
    /// call `verify` for id + signature.
    pub fn from_json(v: &serde_json::Value) -> Result<Claim> {
        let _ = v;
        todo!()
    }

    /// Full JSON including `id` and `sig`.
    pub fn to_json(&self) -> serde_json::Value {
        todo!()
    }

    /// Log line: JCS of `to_json()` + `\n`.
    pub fn to_line(&self) -> Result<Vec<u8>> {
        todo!()
    }

    /// `id == c_(mh(canonical_bytes))` and the Ed25519 signature by `body.actor`
    /// over `SIG_DOMAIN ‖ canonical_bytes` is valid. Failure → `integrity`.
    pub fn verify(&self) -> Result<()> {
        todo!()
    }
}

/// An actor's signing key. The seed is the only secret.
pub struct Signer {
    seed: [u8; 32],
}

impl Signer {
    pub fn generate() -> Result<Signer> {
        todo!()
    }

    pub fn from_seed(seed: [u8; 32]) -> Signer {
        Signer { seed }
    }

    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn actor_id(&self) -> Id {
        todo!()
    }

    /// Validates `body`, requires `body.actor == self.actor_id()`, computes id and sig.
    pub fn seal(&self, body: ClaimBody) -> Result<Claim> {
        let _ = body;
        todo!()
    }
}
