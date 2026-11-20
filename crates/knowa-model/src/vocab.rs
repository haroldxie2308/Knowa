//! Kernel vocabulary (DESIGN §4.6). The `knowa:` namespace is reserved; nothing
//! outside this file may mint `knowa:` terms.

pub const NS: &str = "knowa";

// Policy predicates — their *effect* is gated by author role (DESIGN §4.6, §4.7).
pub const OWNER: &str = "knowa:owner";
pub const ROLE: &str = "knowa:role";
pub const AUTO_ACCEPT: &str = "knowa:autoAccept";
pub const ACTOR_KIND: &str = "knowa:actorKind";
pub const REVIEW: &str = "knowa:review";
pub const RETRACT: &str = "knowa:retract";
pub const VISIBILITY: &str = "knowa:visibility";
pub const DEFAULT_VISIBILITY: &str = "knowa:defaultVisibility";

/// Predicates whose subjects are not entities (they are spaces, actors or claims).
/// The index never materializes an entity from a claim with one of these predicates.
pub const POLICY_PREDICATES: &[&str] =
    &[OWNER, ROLE, AUTO_ACCEPT, ACTOR_KIND, REVIEW, RETRACT, VISIBILITY, DEFAULT_VISIBILITY];

// Descriptive predicates.
pub const TYPE: &str = "knowa:type";
pub const LABEL: &str = "knowa:label";
pub const BODY: &str = "knowa:body";
pub const SUMMARY: &str = "knowa:summary";
pub const CONTENT: &str = "knowa:content";
pub const WHEN: &str = "knowa:when";
pub const WHERE: &str = "knowa:where";
pub const ABOUT: &str = "knowa:about";
pub const MENTIONS: &str = "knowa:mentions";
pub const RELATED: &str = "knowa:related";
pub const SAME_AS: &str = "knowa:sameAs";
pub const NAME: &str = "knowa:name";
pub const SOURCE: &str = "knowa:source";
pub const MEDIA_TYPE: &str = "knowa:mediaType";
pub const SIZE: &str = "knowa:size";

// Terms used as values.
pub const ROLE_OWNER: &str = "knowa:Owner";
pub const ROLE_MEMBER: &str = "knowa:Member";
pub const ROLE_AGENT: &str = "knowa:Agent";
pub const ROLE_WORKER: &str = "knowa:Worker";
pub const ROLE_VIEWER: &str = "knowa:Viewer";

pub const KIND_HUMAN: &str = "knowa:Human";
pub const KIND_AGENT: &str = "knowa:Agent";
pub const KIND_WORKER: &str = "knowa:Worker";

pub const REVIEW_ACCEPT: &str = "knowa:Accept";
pub const REVIEW_REJECT: &str = "knowa:Reject";

pub const VIS_PRIVATE: &str = "knowa:Private";
pub const VIS_SHARED: &str = "knowa:Shared";
pub const VIS_PUBLIC: &str = "knowa:Public";

pub const T_NOTE: &str = "knowa:Note";
pub const T_PHOTO: &str = "knowa:Photo";
pub const T_FILE: &str = "knowa:File";
pub const T_PERSON: &str = "knowa:Person";
pub const T_PLACE: &str = "knowa:Place";
pub const T_EVENT: &str = "knowa:Event";
pub const T_INSIGHT: &str = "knowa:Insight";

/// Domain-separation prefix for claim signatures (DESIGN §4.4).
pub const SIG_DOMAIN: &[u8] = b"knowa/claim/v1\n";

/// On-disk format version written to `knowa.json` and bundles.
pub const FORMAT_VERSION: u32 = 1;
/// Claim schema version (`"v"`).
pub const CLAIM_VERSION: u32 = 1;
