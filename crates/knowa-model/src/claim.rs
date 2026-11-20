//! Claims, drafts and signing (DESIGN §4.4).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde_json::{Map, Value as Json};

use crate::canonical::canonical_json;
use crate::error::{Error, Result};
use crate::id::{Id, IdKind, sha256_multihash_b32};
use crate::term::Term;
use crate::time::{Timestamp, When};
use crate::value::{Dec, MAX_SAFE_INT, Value, dec_cmp};
use crate::vocab::{CLAIM_VERSION, SIG_DOMAIN};

pub const MAX_NOTE_BYTES: usize = 4 * 1024;

/// Fields a writer supplies (a draft) — also the content part of a claim.
const CONTENT_FIELDS: &[&str] = &["s", "p", "o", "q", "valid", "conf", "src", "replaces", "via", "note"];
/// Fields the system assigns.
const ENVELOPE_FIELDS: &[&str] = &["v", "space", "actor", "seq", "prev", "at", "id", "sig"];

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
        let obj = v.as_object().ok_or_else(|| Error::invalid("a draft must be a JSON object"))?;
        for key in obj.keys() {
            if ENVELOPE_FIELDS.contains(&key.as_str()) {
                return Err(Error::invalid(format!("draft field `{key}` is assigned by the system; omit it")));
            }
            if !CONTENT_FIELDS.contains(&key.as_str()) {
                return Err(Error::invalid(format!("unknown draft field `{key}`")));
            }
        }
        let draft = content_from_json(obj)?;
        draft.validate()?;
        Ok(draft)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut m = Map::new();
        self.content().insert_json(&mut m);
        Json::Object(m)
    }

    /// Field-level validation independent of the envelope: value rules, `conf` ∈ [0,1],
    /// `note` ≤ 4 KiB, `replaces` are claim ids (or placeholders are rejected there),
    /// `via` key grammar, every `q` array non-empty.
    pub fn validate(&self) -> Result<()> {
        self.content().validate()
    }

    /// All placeholder ids (`_:x`) appearing in `s`, `o`, `q` refs and `src`.
    /// Each placeholder appears once, in order of first appearance.
    pub fn placeholders(&self) -> Vec<Id> {
        let mut found: Vec<Id> = Vec::new();
        for id in self.content().ids() {
            if id.kind() == IdKind::Placeholder && !found.contains(id) {
                found.push(id.clone());
            }
        }
        found
    }

    /// Replaces placeholders using `map`; a placeholder missing from `map` → `invalid`.
    pub fn resolve(&self, map: &BTreeMap<Id, Id>) -> Result<Draft> {
        let id = |id: &Id| -> Result<Id> {
            if id.kind() != IdKind::Placeholder {
                return Ok(id.clone());
            }
            map.get(id).cloned().ok_or_else(|| Error::invalid(format!("placeholder {id} has no mapping")))
        };
        let value = |v: &Value| -> Result<Value> {
            match v {
                Value::Ref(r) => Ok(Value::Ref(id(r)?)),
                other => Ok(other.clone()),
            }
        };
        Ok(Draft {
            s: id(&self.s)?,
            p: self.p.clone(),
            o: value(&self.o)?,
            q: self
                .q
                .iter()
                .map(|(term, values)| Ok((term.clone(), values.iter().map(value).collect::<Result<_>>()?)))
                .collect::<Result<_>>()?,
            valid: self.valid.clone(),
            conf: self.conf.clone(),
            src: self.src.iter().map(id).collect::<Result<_>>()?,
            replaces: self.replaces.clone(),
            via: self.via.clone(),
            note: self.note.clone(),
        })
    }

    /// Idempotency key (DESIGN §4.4): `mh(JCS({s,p,o,q?,valid?}))` rendered as
    /// lowercase b32 without prefix. Placeholders must already be resolved.
    pub fn equiv_key(&self) -> Result<String> {
        self.content().equiv_key()
    }

    fn content(&self) -> Content<'_> {
        Content {
            s: &self.s,
            p: &self.p,
            o: &self.o,
            q: &self.q,
            valid: self.valid.as_ref(),
            conf: self.conf.as_ref(),
            src: &self.src,
            replaces: &self.replaces,
            via: &self.via,
            note: self.note.as_deref(),
        }
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
        if self.v != CLAIM_VERSION {
            return Err(Error::unsupported(format!("claim version {} (this build supports {CLAIM_VERSION})", self.v)));
        }
        expect_kind(&self.space, IdKind::Space, "space")?;
        expect_kind(&self.actor, IdKind::Actor, "actor")?;
        // Canonical JSON carries only integers up to 2^53 − 1.
        if self.seq == 0 || self.seq > MAX_SAFE_INT.unsigned_abs() {
            return Err(Error::invalid(format!("seq {} is outside 1..=2^53 − 1", self.seq)));
        }
        match (&self.prev, self.seq) {
            (Some(prev), 2..) => expect_kind(prev, IdKind::Claim, "prev")?,
            (None, 1) => {}
            (Some(_), _) => return Err(Error::invalid("prev must be omitted when seq is 1")),
            (None, _) => return Err(Error::invalid(format!("prev is required when seq is {}", self.seq))),
        }
        if !self.at.in_canonical_range() {
            return Err(Error::invalid(format!("at {} is outside years 0000–9999", self.at.format())));
        }
        let content = self.content();
        content.validate()?;
        if let Some(placeholder) = content.ids().find(|id| id.kind() == IdKind::Placeholder) {
            return Err(Error::invalid(format!(
                "placeholder {placeholder} in a claim (placeholders are resolved before sealing)"
            )));
        }
        Ok(())
    }

    /// JSON object without `id`/`sig`; optional fields omitted when empty/None.
    pub fn to_json(&self) -> serde_json::Value {
        Json::Object(self.json_map())
    }

    /// JCS bytes of `to_json()`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        canonical_json(&self.to_json())
    }

    /// Same key as `Draft::equiv_key` for the same {s,p,o,q,valid}.
    pub fn equiv_key(&self) -> Result<String> {
        self.content().equiv_key()
    }

    fn json_map(&self) -> Map<String, Json> {
        let mut m = Map::new();
        m.insert("v".into(), Json::from(self.v));
        m.insert("space".into(), Json::String(self.space.as_str().into()));
        m.insert("actor".into(), Json::String(self.actor.as_str().into()));
        m.insert("seq".into(), Json::from(self.seq));
        if let Some(prev) = &self.prev {
            m.insert("prev".into(), Json::String(prev.as_str().into()));
        }
        m.insert("at".into(), Json::String(self.at.format()));
        self.content().insert_json(&mut m);
        m
    }

    fn content(&self) -> Content<'_> {
        Content {
            s: &self.s,
            p: &self.p,
            o: &self.o,
            q: &self.q,
            valid: self.valid.as_ref(),
            conf: self.conf.as_ref(),
            src: &self.src,
            replaces: &self.replaces,
            via: &self.via,
            note: self.note.as_deref(),
        }
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
        let obj = v.as_object().ok_or_else(|| Error::invalid("a claim must be a JSON object"))?;
        // Version first: a newer claim may carry fields this reader does not know, and
        // the answer for it is `unsupported`, not `invalid`.
        let version = required(obj, "v")?;
        let version = version.as_u64().ok_or_else(|| Error::invalid(format!("`v` must be an integer, got {version}")))?;
        if version != u64::from(CLAIM_VERSION) {
            return Err(Error::unsupported(format!("claim version {version} (this build supports {CLAIM_VERSION})")));
        }
        if let Some(key) =
            obj.keys().find(|k| !CONTENT_FIELDS.contains(&k.as_str()) && !ENVELOPE_FIELDS.contains(&k.as_str()))
        {
            return Err(Error::invalid(format!("unknown claim field `{key}`")));
        }
        let seq = required(obj, "seq")?;
        let body = ClaimBody::from_draft(
            content_from_json(obj)?,
            parse_id(required(obj, "space")?, "space")?,
            parse_id(required(obj, "actor")?, "actor")?,
            seq.as_u64().ok_or_else(|| Error::invalid(format!("`seq` must be a positive integer, got {seq}")))?,
            obj.get("prev").map(|prev| parse_id(prev, "prev")).transpose()?,
            Timestamp::parse(as_str(required(obj, "at")?, "at")?)?,
        );
        body.validate()?;
        let id = parse_id(required(obj, "id")?, "id")?;
        expect_kind(&id, IdKind::Claim, "id")?;
        let sig = as_str(required(obj, "sig")?, "sig")?;
        decode_sig(sig).map_err(|why| Error::invalid(format!("sig: {why}")))?;
        Ok(Claim { body, id, sig: sig.to_string() })
    }

    /// Full JSON including `id` and `sig`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = self.body.json_map();
        m.insert("id".into(), Json::String(self.id.as_str().into()));
        m.insert("sig".into(), Json::String(self.sig.clone()));
        Json::Object(m)
    }

    /// Log line: JCS of `to_json()` + `\n`.
    pub fn to_line(&self) -> Result<Vec<u8>> {
        let mut line = canonical_json(&self.to_json())?;
        line.push(b'\n');
        Ok(line)
    }

    /// `id == c_(mh(canonical_bytes))` and the Ed25519 signature by `body.actor`
    /// over `SIG_DOMAIN ‖ canonical_bytes` is valid. Failure → `integrity`.
    /// A structurally invalid body fails first, with its own code (`invalid`).
    pub fn verify(&self) -> Result<()> {
        self.body.validate()?;
        let canonical = self.body.canonical_bytes()?;
        let computed = Id::claim_of(&canonical);
        if computed != self.id {
            return Err(Error::integrity(format!(
                "claim id {} does not match its content (computed {computed})",
                self.id
            )));
        }
        let actor = &self.body.actor;
        let key = VerifyingKey::from_bytes(&actor.actor_pubkey()?)
            .map_err(|e| Error::integrity(format!("claim {}: actor {actor} is not an Ed25519 key: {e}", self.id)))?;
        let sig = decode_sig(&self.sig).map_err(|why| Error::integrity(format!("claim {}: sig {why}", self.id)))?;
        // Strict verification rejects malleable signatures and small-order keys.
        key.verify_strict(&signed_message(&canonical), &sig).map_err(|e| {
            Error::integrity(format!("claim {}: signature does not verify for actor {actor}: {e}", self.id))
        })
    }
}

/// An actor's signing key. The seed is the only secret.
pub struct Signer {
    seed: [u8; 32],
}

impl Signer {
    pub fn generate() -> Result<Signer> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| Error::internal(format!("OS randomness unavailable: {e}")))?;
        Ok(Signer { seed })
    }

    pub fn from_seed(seed: [u8; 32]) -> Signer {
        Signer { seed }
    }

    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn actor_id(&self) -> Id {
        Id::actor_of(&self.key().verifying_key().to_bytes())
    }

    /// Validates `body`, requires `body.actor == self.actor_id()`, computes id and sig.
    pub fn seal(&self, body: ClaimBody) -> Result<Claim> {
        body.validate()?;
        let actor = self.actor_id();
        if body.actor != actor {
            return Err(Error::invalid(format!("claim actor {} is not this signer's actor {actor}", body.actor)));
        }
        let canonical = body.canonical_bytes()?;
        let sig = self.key().sign(&signed_message(&canonical));
        Ok(Claim { id: Id::claim_of(&canonical), sig: BASE64URL_NOPAD.encode(&sig.to_bytes()), body })
    }

    fn key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.seed)
    }
}

/// The writer-supplied fields shared by `Draft` and `ClaimBody`, borrowed, so both get
/// one implementation of validation, JSON shape and the equivalence key.
#[derive(Clone, Copy)]
struct Content<'a> {
    s: &'a Id,
    p: &'a Term,
    o: &'a Value,
    q: &'a BTreeMap<Term, Vec<Value>>,
    valid: Option<&'a When>,
    conf: Option<&'a Dec>,
    src: &'a [Id],
    replaces: &'a [Id],
    via: &'a BTreeMap<String, String>,
    note: Option<&'a str>,
}

impl<'a> Content<'a> {
    fn validate(self) -> Result<()> {
        self.o.validate().map_err(in_field("o"))?;
        for (term, values) in self.q {
            if values.is_empty() {
                return Err(Error::invalid(format!("q: `{term}` has no values (omit it instead)")));
            }
            for value in values {
                value.validate().map_err(in_field(&format!("q: `{term}`")))?;
            }
        }
        if let Some(valid) = self.valid {
            valid.check_consistent().map_err(in_field("valid"))?;
        }
        if let Some(conf) = self.conf
            && (dec_cmp(conf.as_str(), "0") == Ordering::Less || dec_cmp(conf.as_str(), "1") == Ordering::Greater)
        {
            return Err(Error::invalid(format!("conf {} is outside [0, 1]", conf.as_str())));
        }
        for id in self.replaces {
            expect_kind(id, IdKind::Claim, "replaces")?;
        }
        for key in self.via.keys() {
            let mut bytes = key.bytes();
            let ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase())
                && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !ok {
                return Err(Error::invalid(format!("via key {key:?} must match [a-z][a-z0-9_]*")));
            }
        }
        if let Some(note) = self.note {
            // An empty note would serialize as `"note":""`, a second encoding of "no note".
            if note.is_empty() {
                return Err(Error::invalid("note is empty (omit it instead)"));
            }
            if note.len() > MAX_NOTE_BYTES {
                return Err(Error::invalid(format!("note is {} bytes; the limit is {MAX_NOTE_BYTES}", note.len())));
            }
        }
        Ok(())
    }

    /// Ids in the equivalence key: `s`, `o` and `q` refs.
    fn key_ids(self) -> impl Iterator<Item = &'a Id> {
        std::iter::once(self.s)
            .chain(self.o.as_ref_id())
            .chain(self.q.values().flatten().filter_map(Value::as_ref_id))
    }

    /// Ids where a draft may use placeholders: `s`, `o` and `q` refs, `src`.
    fn ids(self) -> impl Iterator<Item = &'a Id> {
        self.key_ids().chain(self.src)
    }

    fn insert_json(self, m: &mut Map<String, Json>) {
        let ids = |ids: &[Id]| Json::Array(ids.iter().map(|id| Json::String(id.as_str().into())).collect());
        m.insert("s".into(), Json::String(self.s.as_str().into()));
        m.insert("p".into(), Json::String(self.p.as_str().into()));
        m.insert("o".into(), self.o.to_json());
        if !self.q.is_empty() {
            m.insert("q".into(), q_json(self.q));
        }
        if let Some(valid) = self.valid {
            m.insert("valid".into(), Json::String(valid.raw.clone()));
        }
        if let Some(conf) = self.conf {
            m.insert("conf".into(), Json::String(conf.as_str().into()));
        }
        if !self.src.is_empty() {
            m.insert("src".into(), ids(self.src));
        }
        if !self.replaces.is_empty() {
            m.insert("replaces".into(), ids(self.replaces));
        }
        if !self.via.is_empty() {
            let via = self.via.iter().map(|(k, v)| (k.clone(), Json::String(v.clone()))).collect();
            m.insert("via".into(), Json::Object(via));
        }
        if let Some(note) = self.note {
            m.insert("note".into(), Json::String(note.into()));
        }
    }

    fn equiv_key(self) -> Result<String> {
        if let Some(placeholder) = self.key_ids().find(|id| id.kind() == IdKind::Placeholder) {
            return Err(Error::invalid(format!(
                "placeholder {placeholder} must be resolved before computing an equivalence key"
            )));
        }
        let mut m = Map::new();
        m.insert("s".into(), Json::String(self.s.as_str().into()));
        m.insert("p".into(), Json::String(self.p.as_str().into()));
        m.insert("o".into(), self.o.to_json());
        if !self.q.is_empty() {
            m.insert("q".into(), q_json(self.q));
        }
        if let Some(valid) = self.valid {
            m.insert("valid".into(), Json::String(valid.raw.clone()));
        }
        Ok(sha256_multihash_b32(&canonical_json(&Json::Object(m))?))
    }
}

fn q_json(q: &BTreeMap<Term, Vec<Value>>) -> Json {
    Json::Object(
        q.iter()
            .map(|(term, values)| (term.as_str().to_string(), Json::Array(values.iter().map(Value::to_json).collect())))
            .collect(),
    )
}

/// The content fields of a draft or claim (key sets are checked by the caller).
fn content_from_json(obj: &Map<String, Json>) -> Result<Draft> {
    let q = match obj.get("q") {
        None => BTreeMap::new(),
        Some(q) => non_empty_object(q, "q")?
            .iter()
            .map(|(term, values)| {
                let term = Term::parse(term).map_err(in_field("q"))?;
                let field = format!("q: `{term}`");
                let values = non_empty_array(values, &field)?
                    .iter()
                    .map(Value::from_json)
                    .collect::<Result<Vec<_>>>()
                    .map_err(in_field(&field))?;
                Ok((term, values))
            })
            .collect::<Result<_>>()?,
    };
    let via = match obj.get("via") {
        None => BTreeMap::new(),
        Some(via) => non_empty_object(via, "via")?
            .iter()
            .map(|(k, v)| Ok((k.clone(), as_str(v, "via")?.to_string())))
            .collect::<Result<_>>()?,
    };
    Ok(Draft {
        s: parse_id(required(obj, "s")?, "s")?,
        p: Term::parse(as_str(required(obj, "p")?, "p")?).map_err(in_field("p"))?,
        o: Value::from_json(required(obj, "o")?).map_err(in_field("o"))?,
        q,
        valid: obj.get("valid").map(|v| When::parse(as_str(v, "valid")?)).transpose()?,
        conf: obj.get("conf").map(|v| Dec::parse(as_str(v, "conf")?).map_err(in_field("conf"))).transpose()?,
        src: id_list(obj.get("src"), "src")?,
        replaces: id_list(obj.get("replaces"), "replaces")?,
        via,
        note: obj.get("note").map(|v| as_str(v, "note").map(str::to_string)).transpose()?,
    })
}

fn required<'a>(obj: &'a Map<String, Json>, key: &str) -> Result<&'a Json> {
    obj.get(key).ok_or_else(|| Error::invalid(format!("missing required field `{key}`")))
}

fn as_str<'a>(v: &'a Json, field: &str) -> Result<&'a str> {
    v.as_str().ok_or_else(|| Error::invalid(format!("`{field}` must be a string, got {v}")))
}

fn parse_id(v: &Json, field: &str) -> Result<Id> {
    Id::parse(as_str(v, field)?).map_err(in_field(field))
}

fn non_empty_array<'a>(v: &'a Json, field: &str) -> Result<&'a Vec<Json>> {
    match v.as_array() {
        Some(items) if !items.is_empty() => Ok(items),
        Some(_) => Err(Error::invalid(format!("`{field}` is empty (omit it instead)"))),
        None => Err(Error::invalid(format!("`{field}` must be an array, got {v}"))),
    }
}

fn non_empty_object<'a>(v: &'a Json, field: &str) -> Result<&'a Map<String, Json>> {
    match v.as_object() {
        Some(obj) if !obj.is_empty() => Ok(obj),
        Some(_) => Err(Error::invalid(format!("`{field}` is empty (omit it instead)"))),
        None => Err(Error::invalid(format!("`{field}` must be an object, got {v}"))),
    }
}

fn id_list(v: Option<&Json>, field: &str) -> Result<Vec<Id>> {
    match v {
        None => Ok(Vec::new()),
        Some(v) => non_empty_array(v, field)?.iter().map(|id| parse_id(id, field)).collect(),
    }
}

fn expect_kind(id: &Id, kind: IdKind, field: &str) -> Result<()> {
    if id.kind() != kind {
        return Err(Error::invalid(format!("`{field}` must be a {kind:?} id, got {id}")));
    }
    Ok(())
}

/// Prefixes an error's message with the field it came from, keeping its code.
fn in_field(field: &str) -> impl FnOnce(Error) -> Error + '_ {
    move |e| Error { message: format!("{field}: {}", e.message), ..e }
}

fn signed_message(canonical: &[u8]) -> Vec<u8> {
    [SIG_DOMAIN, canonical].concat()
}

fn decode_sig(sig: &str) -> std::result::Result<Signature, String> {
    let bytes = BASE64URL_NOPAD.decode(sig.as_bytes()).map_err(|e| format!("not base64url without padding: {e}"))?;
    Signature::from_slice(&bytes).map_err(|_| format!("expected 64 bytes, got {}", bytes.len()))
}
