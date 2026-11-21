//! SQLite plumbing: schema, error mapping, a small SQL builder and decoding of
//! stored values. Everything stored here was produced by this crate from valid
//! claims, so a value that fails to decode means the index file is corrupt.

use knowa_model::query::MAX_LIMIT;
use knowa_model::{Claim, ClaimView, Error, Id, Result, Status, Term, Value};
use rusqlite::types::Value as SqlValue;

pub(crate) const INDEX_FILE: &str = "index.sqlite";

/// `meta` keys.
pub(crate) const META_SCHEMA: &str = "schema_version";
pub(crate) const META_EPOCH: &str = "epoch";
pub(crate) const META_SPACE: &str = "space";

/// Schema v2. `claims.n` is the local arrival number (DESIGN §6 feed) and the rowid
/// of the claim's `claim_fts` row; `entities.n` is the rowid of `geo`.
///
/// `entities` holds each entity's summary under the DEFAULT view (accepted|proposed,
/// no other filter), the fast path for default-view reads. Any other view derives
/// summaries from the claims that pass it (DESIGN §4.7 "views apply to evidence").
pub(crate) const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;

CREATE TABLE claims(
  n          INTEGER PRIMARY KEY,
  id         TEXT NOT NULL UNIQUE,
  actor      TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  at         INTEGER NOT NULL,
  s          TEXT NOT NULL,
  p          TEXT NOT NULL,
  policy     INTEGER NOT NULL,  -- p is one of vocab::POLICY_PREDICATES
  ent        INTEGER NOT NULL,  -- this claim materializes its subject as an entity
  o          TEXT NOT NULL,     -- canonical JSON of the object value
  o_ref_base TEXT,              -- o.ref without fragment
  conf       TEXT NOT NULL,     -- conf_key(): exact, byte-comparable; "1" when absent
  equiv      TEXT NOT NULL,
  worker     TEXT,              -- via.worker
  json       TEXT NOT NULL,     -- the full claim
  base       TEXT NOT NULL,     -- status with rule 2 (supersession) skipped
  status     TEXT NOT NULL,
  stale      INTEGER NOT NULL
);
CREATE INDEX claims_actor ON claims(actor, seq);
CREATE INDEX claims_s ON claims(s, p);
CREATE INDEX claims_p ON claims(p);
CREATE INDEX claims_oref ON claims(o_ref_base) WHERE o_ref_base IS NOT NULL;
CREATE INDEX claims_at ON claims(at, id);
CREATE INDEX claims_equiv ON claims(equiv, actor, seq);
CREATE INDEX claims_worker ON claims(worker) WHERE worker IS NOT NULL;
-- `where.type`; queries must spell out this partial index's condition literally
CREATE INDEX claims_type ON claims(o, s) WHERE p = 'knowa:type' AND ent = 1;

-- c_ ids listed in a claim's src (stale propagation)
CREATE TABLE src(claim TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(claim, target)) WITHOUT ROWID;
CREATE INDEX src_target ON src(target);
-- ids listed in a claim's replaces (supersession)
CREATE TABLE replaces(claim TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(claim, target)) WITHOUT ROWID;
CREATE INDEX replaces_target ON replaces(target);
-- base blob ids referenced by a claim in s, o.ref, src or q refs (export)
CREATE TABLE blob_refs(claim TEXT NOT NULL, blob TEXT NOT NULL, PRIMARY KEY(claim, blob)) WITHOUT ROWID;

-- computed policy state (policy::PolicyState)
CREATE TABLE actors(
  id     TEXT PRIMARY KEY,
  kind   TEXT,
  role   TEXT,
  owner  INTEGER NOT NULL,
  member INTEGER NOT NULL,
  auto   INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE entities(
  n           INTEGER PRIMARY KEY,
  id          TEXT NOT NULL UNIQUE,
  first_at    INTEGER,           -- earliest record time of a passing claim; NULL if none passes
  label       TEXT,
  types       TEXT NOT NULL,     -- JSON array of terms
  when_value  TEXT,
  when_lo     INTEGER,
  when_hi     INTEGER,
  when_source TEXT,
  when_key    INTEGER,           -- ordering key for `when` / `-when`
  lat         REAL,
  lon         REAL,
  r           REAL,
  claims      TEXT NOT NULL      -- JSON array of supporting claim ids
);

-- one document per entity-materializing claim with text (entity::own_text, plus the
-- text of the blob it links, entity::linked_blob), of any status: the view picks the
-- rows that count at query time
CREATE VIRTUAL TABLE claim_fts USING fts5(body, content='', contentless_delete=1, tokenize='unicode61');
CREATE VIRTUAL TABLE geo USING rtree(n, min_lat, max_lat, min_lon, max_lon);
"#;

pub(crate) fn db(e: rusqlite::Error) -> Error {
    Error::internal(format!("index database: {e}"))
}

pub(crate) fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::internal(format!("index corrupt ({what}); run `knowa reindex`"))
}

pub(crate) fn status_from(s: &str) -> Result<Status> {
    match s {
        "accepted" => Ok(Status::Accepted),
        "proposed" => Ok(Status::Proposed),
        "rejected" => Ok(Status::Rejected),
        "retracted" => Ok(Status::Retracted),
        "superseded" => Ok(Status::Superseded),
        other => Err(corrupt(format_args!("unknown status {other:?}"))),
    }
}

pub(crate) fn id_from(s: &str) -> Result<Id> {
    Id::parse(s).map_err(|e| corrupt(format_args!("stored id {s:?}: {}", e.message)))
}

pub(crate) fn term_from(s: &str) -> Result<Term> {
    Term::parse(s).map_err(|e| corrupt(format_args!("stored term {s:?}: {}", e.message)))
}

pub(crate) fn value_from(json: &str) -> Result<Value> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| corrupt(format_args!("stored value: {e}")))?;
    Value::from_json(&v).map_err(|e| corrupt(format_args!("stored value: {}", e.message)))
}

pub(crate) fn claim_view(json: &str, status: &str, stale: i64) -> Result<ClaimView> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| corrupt(format_args!("stored claim: {e}")))?;
    let claim = Claim::from_json(&v).map_err(|e| corrupt(format_args!("stored claim: {}", e.message)))?;
    Ok(ClaimView { claim, status: status_from(status)?, stale: stale != 0 })
}

/// Reads `json, status, stale` at columns `at..at+3` of a row.
pub(crate) fn claim_view_at(row: &rusqlite::Row<'_>, at: usize) -> Result<ClaimView> {
    let json: String = row.get(at).map_err(db)?;
    let status: String = row.get(at + 1).map_err(db)?;
    let stale: i64 = row.get(at + 2).map_err(db)?;
    claim_view(&json, &status, stale)
}

pub(crate) fn i64_of(n: u64, what: &str) -> Result<i64> {
    i64::try_from(n).map_err(|_| Error::invalid(format!("{what} {n} exceeds the index's integer range")))
}

pub(crate) fn u64_of(n: i64, what: &str) -> Result<u64> {
    u64::try_from(n).map_err(|_| corrupt(format_args!("negative {what} {n}")))
}

/// Canonical JSON text of a value: equal values ⇔ equal strings.
pub(crate) fn canonical(v: &Value) -> Result<String> {
    let bytes = knowa_model::canonical::canonical_json(&v.to_json())?;
    String::from_utf8(bytes).map_err(|e| Error::internal(format!("canonical JSON is not UTF-8: {e}")))
}

/// Exact, byte-comparable key of a decimal in [0, 1]: `"0"`, `"1"` or `"0.<digits>"`
/// without trailing zeros. Lexicographic order of keys equals numeric order, so
/// `minConf` is an exact SQL comparison (no float rounding).
pub(crate) fn conf_key(dec: &str) -> Result<String> {
    let out_of_range = || Error::invalid(format!("confidence {dec:?} is outside [0, 1]"));
    let (negative, unsigned) = match dec.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, dec),
    };
    let (int, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let well_formed = !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && (frac.is_empty() != unsigned.contains('.'));
    if !well_formed {
        return Err(Error::invalid(format!("confidence {dec:?} is not a decimal")));
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    if negative && !(int.is_empty() && frac.is_empty()) {
        return Err(out_of_range());
    }
    match (int, frac) {
        ("", "") => Ok("0".to_string()),
        ("", f) => Ok(format!("0.{f}")),
        ("1", "") => Ok("1".to_string()),
        _ => Err(out_of_range()),
    }
}

/// Page size check shared by every paged read.
pub(crate) fn check_limit(limit: u32) -> Result<i64> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(Error::invalid(format!("limit must be between 1 and {MAX_LIMIT}, got {limit}")));
    }
    Ok(i64::from(limit))
}

const CURSOR_PREFIX: &str = "off:";

/// This index's opaque page cursor (v0: an offset).
pub(crate) fn parse_cursor(cursor: Option<&str>) -> Result<u64> {
    let Some(c) = cursor else { return Ok(0) };
    c.strip_prefix(CURSOR_PREFIX)
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| Error::invalid(format!("malformed cursor {c:?}")))
}

pub(crate) fn make_cursor(offset: u64) -> String {
    format!("{CURSOR_PREFIX}{offset}")
}

/// SQL text with positional `?` parameters, appended together so their order
/// can never drift apart.
pub(crate) struct Sql {
    pub text: String,
    pub params: Vec<SqlValue>,
}

impl Sql {
    pub fn new(text: &str) -> Sql {
        Sql { text: text.to_string(), params: Vec::new() }
    }

    pub fn add(&mut self, text: &str) -> &mut Sql {
        self.text.push_str(text);
        self
    }

    pub fn arg(&mut self, v: impl Into<SqlValue>) -> &mut Sql {
        self.text.push('?');
        self.params.push(v.into());
        self
    }

    /// `(?, ?, …)`; callers never pass an empty list.
    pub fn list<I, V>(&mut self, items: I) -> &mut Sql
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.text.push('(');
        for (i, v) in items.into_iter().enumerate() {
            if i > 0 {
                self.text.push_str(", ");
            }
            self.arg(v);
        }
        self.text.push(')');
        self
    }
}
