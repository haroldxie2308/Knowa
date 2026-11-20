//! The query document and its results (DESIGN §6.1, §6.2). Serde shapes here ARE
//! the public contract: evolve additively only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::Id;
use crate::term::Term;
use crate::value::Value;
use crate::view::{ActorInfo, ActorKind, BlobView, ClaimView, Dir, EntityView, Status};

pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 1000;
pub const MAX_HOPS: u32 = 3;
pub const MAX_PATH_HOPS: u32 = 6;

/// Trust filter applied at read time. Default: accepted + proposed, stale included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct View {
    #[serde(default = "View::default_status")]
    pub status: Vec<Status>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actor_kinds: Vec<ActorKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<Id>,
    #[serde(default = "View::default_include_stale")]
    pub include_stale: bool,
    /// decimal string in [0,1]; claims without `conf` count as 1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_conf: Option<String>,
}

impl View {
    fn default_status() -> Vec<Status> {
        vec![Status::Accepted, Status::Proposed]
    }
    fn default_include_stale() -> bool {
        true
    }
}

impl Default for View {
    fn default() -> Self {
        View {
            status: View::default_status(),
            actor_kinds: Vec::new(),
            actors: Vec::new(),
            include_stale: true,
            min_conf: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhenRange {
    /// EDTF; the range's lo
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// EDTF; the range's hi
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Near {
    pub lat: String,
    pub lon: String,
    pub km: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NearId {
    pub id: Id,
    pub km: String,
}

/// Conjunctive filters. Every present field must match. For `target: entities`,
/// claim-level filters (`s`,`p`,`o`,`actors`,`recorded*`) match if ANY claim about
/// the entity satisfies all claim-level filters together with the view.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Where {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub s: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub p: Vec<Term>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub o: Option<Value>,
    /// entity has `knowa:type` with any of these
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "type")]
    pub type_: Vec<Term>,
    /// full-text query (FTS syntax is NOT exposed: the string is tokenized and
    /// all tokens must match, prefix-matching the last token)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// entity `when` range overlaps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<WhenRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Near>,
    /// resolved to `near` by the api layer before reaching an index
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near_id: Option<NearId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<Id>,
    /// RFC 3339
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_before: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Entities,
    Claims,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Order {
    /// bm25 when `text` present, else `-when` (entities) / `-recorded` (claims)
    #[serde(rename = "relevance")]
    Relevance,
    #[serde(rename = "when")]
    When,
    #[serde(rename = "-when")]
    WhenDesc,
    #[serde(rename = "recorded")]
    Recorded,
    #[serde(rename = "-recorded")]
    RecordedDesc,
    /// requires `near`/`nearId`
    #[serde(rename = "distance")]
    Distance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FindDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub view: View,
    /// reserved (DESIGN §6.1): any non-null value → `unsupported`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<serde_json::Value>,
    #[serde(default)]
    pub target: Target,
    #[serde(default, rename = "where")]
    pub where_: Where,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GetDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<serde_json::Value>,
    pub ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContextDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<serde_json::Value>,
    pub id: Id,
    /// graph neighborhood depth, 0..=MAX_HOPS, default 1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hops: Option<u32>,
    /// ISO 8601 duration for temporal neighbors, default "P1D"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// radius for spatial neighbors, default "1"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km: Option<String>,
    /// per-section cap, default 20
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PathDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<serde_json::Value>,
    pub from: Id,
    pub to: Id,
    /// default 4, max MAX_PATH_HOPS
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hops: Option<u32>,
    /// max paths returned, default 3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LogDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<Id>,
    /// RFC 3339 record-time lower bound (inclusive)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum QueryDoc {
    Find(FindDoc),
    Get(GetDoc),
    Context(ContextDoc),
    Path(PathDoc),
    Log(LogDoc),
}

// ---- results ----

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FindItems {
    Entities(Vec<EntityView>),
    Claims(Vec<ClaimView>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FindResult {
    pub items: FindItems,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum GetItem {
    Entity { entity: EntityView, claims: Vec<ClaimView> },
    Claim { claim: ClaimView },
    Blob { blob: BlobView },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GetResult {
    pub items: Vec<GetItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Neighbor {
    pub entity: EntityView,
    pub via: Id,
    pub p: Term,
    pub dir: Dir,
    /// 1-based graph distance from the context entity
    pub hop: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextResult {
    pub entity: EntityView,
    /// claims with s = id
    pub claims: Vec<ClaimView>,
    /// claims with o.ref = id
    pub inbound: Vec<ClaimView>,
    pub neighbors: Vec<Neighbor>,
    /// entities whose `when` overlaps the anchor's widened by `window`, nearest first
    pub temporal: Vec<EntityView>,
    /// entities within `km` of the anchor's `where`, nearest first
    pub spatial: Vec<EntityView>,
    pub actors: Vec<ActorInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathStep {
    pub id: Id,
    /// the claim that links the previous step to this one (None on the first step)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<Term>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<Dir>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathResult {
    pub paths: Vec<Vec<PathStep>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogResult {
    pub items: Vec<ClaimView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum QueryResult {
    Find(FindResult),
    Get(GetResult),
    Context(ContextResult),
    Path(PathResult),
    Log(LogResult),
}

// ---- write / feed / blob results ----

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteItem {
    pub id: Id,
    /// false when an equivalent claim by the same actor already existed (DESIGN §4.4)
    pub created: bool,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteResult {
    pub items: Vec<WriteItem>,
    /// placeholder → minted entity id
    pub placeholders: BTreeMap<String, Id>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeedResult {
    pub claims: Vec<ClaimView>,
    /// `<epoch>:<n>`; pass back to continue
    pub cursor: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlobPutResult {
    pub id: Id,
    pub created: bool,
    pub size: u64,
}
