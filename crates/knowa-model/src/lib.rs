//! Knowa kernel: identifiers, values, claims, canonical form, signing and the
//! read/query contract types. Every other crate depends on this one; nothing here
//! touches the filesystem.

pub mod canonical;
pub mod claim;
pub mod error;
pub mod id;
pub mod query;
pub mod term;
pub mod time;
pub mod value;
pub mod view;
pub mod vocab;

pub use claim::{Claim, ClaimBody, Draft, Signer};
pub use error::{Error, ErrorCode, Result};
pub use id::{Id, IdKind};
pub use term::Term;
pub use time::{Timestamp, When};
pub use value::{Dec, Geo, Value};
pub use view::{ActorInfo, ActorKind, BlobView, ClaimView, Dir, Edge, EntityView, Role, Status, Visibility};
