//! The status rules of DESIGN §4.6–4.7, as pure functions. This is the only place
//! that decides what a retract, a review or a replacement means; `policy` and
//! `apply` only gather the inputs.

use knowa_model::vocab;
use knowa_model::{ActorKind, Role, Status, Value, Visibility};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Accept,
    Reject,
}

/// `knowa:retract` (o must be `{"bool":true}`) is effective iff its author is the
/// target's author or an owner, and it is not itself retracted.
pub(crate) fn retract_effective(o: &Value, by_target_author: bool, by_owner: bool, retract_retracted: bool) -> bool {
    *o == Value::Bool(true) && (by_target_author || by_owner) && !retract_retracted
}

/// The verdict of a `knowa:review` value; any other term is not a review.
pub(crate) fn verdict(o: &Value) -> Option<Verdict> {
    match o {
        Value::Term(t) if t.as_str() == vocab::REVIEW_ACCEPT => Some(Verdict::Accept),
        Value::Term(t) if t.as_str() == vocab::REVIEW_REJECT => Some(Verdict::Reject),
        _ => None,
    }
}

/// A review is effective iff its author is an owner or member and it is not retracted.
pub(crate) fn review_effective(by_reviewer: bool, review_retracted: bool) -> bool {
    by_reviewer && !review_retracted
}

/// A claim r that lists c in `replaces` supersedes c iff r's *base* status is accepted
/// (DESIGN §4.7 rule 2): a proposed replacement, or one that is only superseded
/// itself, supersedes nothing.
pub(crate) fn supersedes(replacer_base: Status) -> bool {
    replacer_base == Status::Accepted
}

/// The base status: DESIGN §4.7 with rule 2 skipped (rules 1, 3, 4, 5, first match
/// wins). `review` is the latest effective review.
pub(crate) fn base_status(retracted: bool, review: Option<Verdict>, author_accepted: bool) -> Status {
    if retracted {
        Status::Retracted
    } else if let Some(v) = review {
        match v {
            Verdict::Accept => Status::Accepted,
            Verdict::Reject => Status::Rejected,
        }
    } else if author_accepted {
        Status::Accepted
    } else {
        Status::Proposed
    }
}

/// The status: rule 2 inserted into the base status's rule order. Only rule 1
/// (retracted) precedes it.
pub(crate) fn status(base: Status, superseded: bool) -> Status {
    if superseded && base != Status::Retracted { Status::Superseded } else { base }
}

/// A `src` input makes its dependent stale iff it is rejected/retracted/superseded
/// or itself stale.
pub(crate) fn makes_stale(input: Status, input_stale: bool) -> bool {
    input.invalidates_dependents() || input_stale
}

pub(crate) fn role_of(term: &str) -> Option<Role> {
    match term {
        vocab::ROLE_OWNER => Some(Role::Owner),
        vocab::ROLE_MEMBER => Some(Role::Member),
        vocab::ROLE_AGENT => Some(Role::Agent),
        vocab::ROLE_WORKER => Some(Role::Worker),
        vocab::ROLE_VIEWER => Some(Role::Viewer),
        _ => None,
    }
}

pub(crate) fn kind_of(term: &str) -> Option<ActorKind> {
    match term {
        vocab::KIND_HUMAN => Some(ActorKind::Human),
        vocab::KIND_AGENT => Some(ActorKind::Agent),
        vocab::KIND_WORKER => Some(ActorKind::Worker),
        _ => None,
    }
}

pub(crate) fn visibility_of(o: &Value) -> Option<Visibility> {
    match o {
        Value::Term(t) => match t.as_str() {
            vocab::VIS_PRIVATE => Some(Visibility::Private),
            vocab::VIS_SHARED => Some(Visibility::Shared),
            vocab::VIS_PUBLIC => Some(Visibility::Public),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_policy(p: &str) -> bool {
    vocab::POLICY_PREDICATES.contains(&p)
}
