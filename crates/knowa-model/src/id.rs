//! Identifiers (DESIGN §4.1). One `Id` type for all kinds; positions that require a
//! specific kind check `kind()` during validation.

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdKind {
    /// `c_` + b32(multihash sha2-256 of canonical claim bytes)
    Claim,
    /// `b_` + b32(multihash sha2-256 of bytes), optionally followed by `#fragment`
    Blob,
    /// `a_` + b32(0xed 0x01 ‖ ed25519 pubkey)
    Actor,
    /// `e_` + lowercase ULID
    Entity,
    /// `s_` + lowercase ULID
    Space,
    /// `x:<ns>:<key>`
    External,
    /// `_:<name>` — only legal inside a write batch's drafts; never in a stored claim.
    Placeholder,
}

/// A validated identifier. Invariant: `Id::parse(id.as_str()) == Ok(id)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(String);

impl Id {
    /// Validates syntax for every kind, including b32 alphabet/length for hashed kinds,
    /// ULID alphabet/length for `e_`/`s_`, fragment grammar for `b_…#…` (DESIGN §4.1),
    /// `x:` ns/key grammar, and `_:` placeholder names `[A-Za-z0-9_-]+`.
    /// Rejects fragments on any kind other than Blob.
    pub fn parse(s: &str) -> Result<Id> {
        let _ = s;
        todo!()
    }

    pub fn kind(&self) -> IdKind {
        todo!()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fragment after `#` (Blob only), without the `#`.
    pub fn fragment(&self) -> Option<&str> {
        todo!()
    }

    /// The id with any fragment removed (identity for non-blob kinds).
    pub fn base(&self) -> Id {
        todo!()
    }

    /// `b_` id of `bytes`.
    pub fn blob_of(bytes: &[u8]) -> Id {
        let _ = bytes;
        todo!()
    }

    /// `b_` id from an already-computed sha256 digest (for streaming hashing).
    pub fn blob_from_sha256(digest: &[u8; 32]) -> Id {
        let _ = digest;
        todo!()
    }

    /// `c_` id of canonical claim bytes.
    pub fn claim_of(canonical: &[u8]) -> Id {
        let _ = canonical;
        todo!()
    }

    pub fn actor_of(pubkey: &[u8; 32]) -> Id {
        let _ = pubkey;
        todo!()
    }

    /// Recovers the ed25519 public key from an `a_` id.
    pub fn actor_pubkey(&self) -> Result<[u8; 32]> {
        todo!()
    }

    /// Fresh `e_` id (ULID: 48-bit ms timestamp + 80 bits of OS randomness).
    pub fn new_entity() -> Id {
        todo!()
    }

    /// Fresh `s_` id.
    pub fn new_space() -> Id {
        todo!()
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Id::parse(&s).map_err(|e| serde::de::Error::custom(e.message))
    }
}
