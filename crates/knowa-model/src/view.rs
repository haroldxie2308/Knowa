//! Read-side contract types (DESIGN §4.7, §6.2). These are what agents see.

use serde::{Deserialize, Serialize};

use crate::claim::Claim;
use crate::id::Id;
use crate::term::Term;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Accepted,
    Proposed,
    Rejected,
    Retracted,
    Superseded,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Accepted => "accepted",
            Status::Proposed => "proposed",
            Status::Rejected => "rejected",
            Status::Retracted => "retracted",
            Status::Superseded => "superseded",
        }
    }
    /// Statuses that make a claim an invalid input for `stale` propagation (DESIGN §4.7).
    pub fn invalidates_dependents(self) -> bool {
        matches!(self, Status::Rejected | Status::Retracted | Status::Superseded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "knowa:Owner")]
    Owner,
    #[serde(rename = "knowa:Member")]
    Member,
    #[serde(rename = "knowa:Agent")]
    Agent,
    #[serde(rename = "knowa:Worker")]
    Worker,
    #[serde(rename = "knowa:Viewer")]
    Viewer,
}

impl Role {
    /// The kernel term for this role (DESIGN §4.6).
    pub fn as_term(self) -> &'static str {
        match self {
            Role::Owner => crate::vocab::ROLE_OWNER,
            Role::Member => crate::vocab::ROLE_MEMBER,
            Role::Agent => crate::vocab::ROLE_AGENT,
            Role::Worker => crate::vocab::ROLE_WORKER,
            Role::Viewer => crate::vocab::ROLE_VIEWER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActorKind {
    #[serde(rename = "knowa:Human")]
    Human,
    #[serde(rename = "knowa:Agent")]
    Agent,
    #[serde(rename = "knowa:Worker")]
    Worker,
}

impl ActorKind {
    /// The kernel term for this kind (DESIGN §4.6).
    pub fn as_term(self) -> &'static str {
        match self {
            ActorKind::Human => crate::vocab::KIND_HUMAN,
            ActorKind::Agent => crate::vocab::KIND_AGENT,
            ActorKind::Worker => crate::vocab::KIND_WORKER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Visibility {
    #[serde(rename = "knowa:Private")]
    Private,
    #[serde(rename = "knowa:Shared")]
    Shared,
    #[serde(rename = "knowa:Public")]
    Public,
}

/// A stored claim plus its computed state. Serialized as the claim's JSON with
/// `status` and `stale` added (the claim's own `space` field identifies the space).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimView {
    pub claim: Claim,
    pub status: Status,
    pub stale: bool,
}

impl Serialize for ClaimView {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut v = self.claim.to_json();
        v["status"] = serde_json::Value::String(self.status.as_str().to_string());
        v["stale"] = serde_json::Value::Bool(self.stale);
        v.serialize(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenSource {
    /// from a `knowa:when` claim
    Claim,
    /// fallback: earliest record time of claims about the entity (DESIGN §4.8)
    Recorded,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityWhen {
    /// EDTF as written, or the canonical timestamp for `recorded`
    pub value: String,
    /// canonical timestamp; None = open
    pub from: Option<String>,
    pub to: Option<String>,
    pub source: WhenSource,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityWhere {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<f64>,
}

/// Summary of an entity. Display fields follow DESIGN §4.7 "display value".
/// v0: summary fields are computed from claims with status accepted|proposed,
/// independent of the query view; the view decides which entities match.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityView {
    pub id: Id,
    pub spaces: Vec<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub types: Vec<Term>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<EntityWhen>,
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_: Option<EntityWhere>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(rename = "distanceKm", skip_serializing_if = "Option::is_none")]
    pub distance_km: Option<f64>,
    /// ids of the claims that supplied label/types/when/where
    pub claims: Vec<Id>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Out,
    In,
}

/// One graph edge from the perspective of `from`: an active ref-valued claim.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub from: Id,
    pub to: Id,
    pub p: Term,
    pub via: Id,
    pub dir: Dir,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActorInfo {
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ActorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(rename = "autoAccept")]
    pub auto_accept: bool,
    pub claims: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlobView {
    pub id: Id,
    pub size: u64,
    #[serde(rename = "mediaType", skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub spaces: Vec<Id>,
}
