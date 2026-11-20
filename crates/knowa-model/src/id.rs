//! Identifiers (DESIGN §4.1). One `Id` type for all kinds; positions that require a
//! specific kind check `kind()` during validation.

use std::cmp::Ordering;

use data_encoding::BASE32_NOPAD;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::value::dec_cmp;

/// Multihash header for sha2-256: code 0x12, digest length 0x20.
const MH_SHA256: [u8; 2] = [0x12, 0x20];
/// Multicodec header for an ed25519 public key.
const MC_ED25519_PUB: [u8; 2] = [0xed, 0x01];
/// b32 length of a 2-byte header plus a 32-byte payload: ceil(34 * 8 / 5).
const HASHED_B32_LEN: usize = 55;
/// Crockford base32, lowercase (no i, l, o, u).
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const ULID_LEN: usize = 26;

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
        check_id(s).map_err(|why| Error::invalid(format!("invalid id {s:?}: {why}")))?;
        Ok(Id(s.to_string()))
    }

    pub fn kind(&self) -> IdKind {
        match prefix_kind(&self.0) {
            Some(kind) => kind,
            // Invariant: an `Id` is only built by `parse` (which rejects unknown prefixes)
            // or by the constructors below, which write a known prefix.
            None => unreachable!("Id {:?} has no known prefix", self.0),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fragment after `#` (Blob only), without the `#`.
    pub fn fragment(&self) -> Option<&str> {
        match self.kind() {
            IdKind::Blob => self.0.split_once('#').map(|(_, frag)| frag),
            _ => None,
        }
    }

    /// The id with any fragment removed (identity for non-blob kinds).
    pub fn base(&self) -> Id {
        match (self.kind(), self.0.split_once('#')) {
            (IdKind::Blob, Some((base, _))) => Id(base.to_string()),
            _ => self.clone(),
        }
    }

    /// `b_` id of `bytes`.
    pub fn blob_of(bytes: &[u8]) -> Id {
        Id(format!("b_{}", sha256_multihash_b32(bytes)))
    }

    /// `b_` id from an already-computed sha256 digest (for streaming hashing).
    pub fn blob_from_sha256(digest: &[u8; 32]) -> Id {
        Id(format!("b_{}", b32_encode(&[&MH_SHA256[..], digest].concat())))
    }

    /// `c_` id of canonical claim bytes.
    pub fn claim_of(canonical: &[u8]) -> Id {
        Id(format!("c_{}", sha256_multihash_b32(canonical)))
    }

    pub fn actor_of(pubkey: &[u8; 32]) -> Id {
        Id(format!("a_{}", b32_encode(&[&MC_ED25519_PUB[..], pubkey].concat())))
    }

    /// Recovers the ed25519 public key from an `a_` id.
    pub fn actor_pubkey(&self) -> Result<[u8; 32]> {
        if self.kind() != IdKind::Actor {
            return Err(Error::invalid(format!("{} is not an actor id", self.0)));
        }
        // `parse`/`actor_of` guarantee 34 bytes with the ed25519-pub header; a failure
        // below means that invariant broke, which is a bug, not bad input.
        let broken = || Error::internal(format!("actor id {} does not hold an ed25519 key", self.0));
        let bytes = b32_decode(&self.0[2..]).map_err(|e| Error::internal(format!("actor id {}: {e}", self.0)))?;
        let key = bytes.strip_prefix(&MC_ED25519_PUB[..]).ok_or_else(broken)?;
        <[u8; 32]>::try_from(key).map_err(|_| broken())
    }

    /// Fresh `e_` id (ULID: 48-bit ms timestamp + 80 bits of OS randomness).
    /// OS randomness unavailable → `internal`.
    pub fn new_entity() -> Result<Id> {
        Ok(Id(format!("e_{}", new_ulid()?)))
    }

    /// Fresh `s_` id. OS randomness unavailable → `internal`.
    pub fn new_space() -> Result<Id> {
        Ok(Id(format!("s_{}", new_ulid()?)))
    }
}

/// Lowercase Crockford ULID: 48-bit ms timestamp (big-endian) ‖ 80 random bits.
fn new_ulid() -> Result<String> {
    let mut rand = [0u8; 10];
    getrandom::fill(&mut rand).map_err(|e| Error::internal(format!("OS randomness unavailable: {e}")))?;
    // ULID time is unsigned 48-bit; a clock before 1970 or after year 10889 cannot be encoded.
    let ms = u64::try_from(crate::time::Timestamp::now().0)
        .ok()
        .filter(|ms| *ms < 1 << 48)
        .ok_or_else(|| Error::internal("system clock outside the ULID range (1970..10889)"))?;
    let random = rand.iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    let n = (u128::from(ms) << 80) | random;
    // 26 base32 digits, most significant first (the top digit carries the 2 spare bits).
    Ok((0..ULID_LEN).rev().map(|i| char::from(CROCKFORD[((n >> (5 * i)) & 0x1f) as usize])).collect())
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

/// RFC 4648 base32, lowercase, no padding.
pub(crate) fn b32_encode(bytes: &[u8]) -> String {
    let mut s = BASE32_NOPAD.encode(bytes);
    s.make_ascii_lowercase();
    s
}

/// Strict inverse of `b32_encode`: lowercase alphabet only, no padding, zero trailing
/// bits — so every byte string has exactly one accepted textual form.
pub(crate) fn b32_decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    if let Some(c) = s.chars().find(|c| !matches!(c, 'a'..='z' | '2'..='7')) {
        return Err(format!("{c:?} is not in the lowercase base32 alphabet (no padding)"));
    }
    BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes()).map_err(|e| format!("base32: {e}"))
}

/// b32(multihash sha2-256(bytes)), without any kind prefix.
pub(crate) fn sha256_multihash_b32(bytes: &[u8]) -> String {
    b32_encode(&[&MH_SHA256[..], &Sha256::digest(bytes)[..]].concat())
}

fn prefix_kind(s: &str) -> Option<IdKind> {
    match s.get(..2)? {
        "c_" => Some(IdKind::Claim),
        "b_" => Some(IdKind::Blob),
        "a_" => Some(IdKind::Actor),
        "e_" => Some(IdKind::Entity),
        "s_" => Some(IdKind::Space),
        "x:" => Some(IdKind::External),
        "_:" => Some(IdKind::Placeholder),
        _ => None,
    }
}

fn check_id(s: &str) -> std::result::Result<(), String> {
    let kind = prefix_kind(s).ok_or("unknown prefix (expected c_, b_, a_, e_, s_, x: or _:)")?;
    // `prefix_kind` matched two ASCII bytes, so index 2 is a char boundary.
    let rest = &s[2..];
    match kind {
        IdKind::Blob => {
            let (hash, fragment) = match rest.split_once('#') {
                Some((hash, fragment)) => (hash, Some(fragment)),
                None => (rest, None),
            };
            check_hashed(hash, MH_SHA256)?;
            fragment.map_or(Ok(()), check_fragment)
        }
        // An `x:` key is a foreign key owned by another system (e.g. a URL), so `#` is
        // part of the key there, not a fragment.
        IdKind::External => check_external(rest),
        _ if rest.contains('#') => Err("fragments are only allowed on b_ ids".into()),
        IdKind::Claim => check_hashed(rest, MH_SHA256),
        IdKind::Actor => check_hashed(rest, MC_ED25519_PUB),
        IdKind::Entity | IdKind::Space => check_ulid(rest),
        IdKind::Placeholder => check_placeholder(rest),
    }
}

fn check_hashed(b32: &str, header: [u8; 2]) -> std::result::Result<(), String> {
    if b32.len() != HASHED_B32_LEN {
        return Err(format!("expected {HASHED_B32_LEN} base32 characters, got {}", b32.len()));
    }
    let bytes = b32_decode(b32)?;
    if bytes.len() != 34 || bytes[..2] != header {
        return Err(format!("expected header {:02x}{:02x}", header[0], header[1]));
    }
    Ok(())
}

fn check_ulid(s: &str) -> std::result::Result<(), String> {
    if s.len() != ULID_LEN {
        return Err(format!("ULID must be {ULID_LEN} characters, got {}", s.len()));
    }
    if let Some(c) = s.bytes().find(|b| !CROCKFORD.contains(b)) {
        return Err(format!("{:?} is not in the lowercase Crockford base32 alphabet", c as char));
    }
    // 26 chars carry 130 bits; a ULID is 128, so the first char is at most '7'.
    if s.as_bytes()[0] > b'7' {
        return Err("ULID overflows 128 bits (first character must be 0-7)".into());
    }
    Ok(())
}

fn check_external(s: &str) -> std::result::Result<(), String> {
    let (ns, key) = s.split_once(':').ok_or("expected x:<ns>:<key>")?;
    if ns.is_empty() || !ns.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')) {
        return Err("x: namespace must match [a-z0-9-]+".into());
    }
    if key.is_empty() {
        return Err("x: key must be non-empty".into());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("x: key must not contain whitespace".into());
    }
    Ok(())
}

fn check_placeholder(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err("placeholder name must match [A-Za-z0-9_-]+".into());
    }
    Ok(())
}

/// `xywh=x,y,w,h` (pixels) · `t=start,end` (seconds) · `page=n` · `char=start,end`.
/// Numbers have one textual form (no leading zeros, no trailing fractional zeros) so
/// that equal fragments compare equal as strings; ranges must be non-empty.
fn check_fragment(fragment: &str) -> std::result::Result<(), String> {
    let (name, args) = fragment.split_once('=').ok_or("fragment must be name=value")?;
    let args: Vec<&str> = args.split(',').collect();
    match (name, args.as_slice()) {
        ("xywh", [x, y, w, h]) => {
            for n in [x, y] {
                frag_uint(n)?;
            }
            for n in [w, h] {
                if frag_uint(n)? == 0 {
                    return Err("xywh width and height must be positive".into());
                }
            }
            Ok(())
        }
        ("t", [start, end]) => {
            frag_seconds(start)?;
            frag_seconds(end)?;
            if dec_cmp(start, end) != Ordering::Less {
                return Err("t start must be before end".into());
            }
            Ok(())
        }
        ("page", [n]) => {
            if frag_uint(n)? == 0 {
                return Err("page numbers start at 1".into());
            }
            Ok(())
        }
        ("char", [start, end]) => {
            if frag_uint(start)? >= frag_uint(end)? {
                return Err("char start must be before end".into());
            }
            Ok(())
        }
        ("xywh" | "t" | "page" | "char", _) => Err(format!("wrong number of values for fragment {name:?}")),
        _ => Err(format!("unknown fragment {name:?} (expected xywh, t, page or char)")),
    }
}

/// `0|[1-9][0-9]*`, fitting in u64.
fn frag_uint(s: &str) -> std::result::Result<u64, String> {
    let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if !canonical {
        return Err(format!("{s:?} is not a non-negative integer without leading zeros"));
    }
    s.parse::<u64>().map_err(|e| format!("{s:?}: {e}"))
}

/// `(0|[1-9][0-9]*)(\.[0-9]*[1-9])?`
fn frag_seconds(s: &str) -> std::result::Result<(), String> {
    let (int, frac) = match s.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (s, None),
    };
    let int_ok = !int.is_empty() && int.bytes().all(|b| b.is_ascii_digit()) && (int == "0" || !int.starts_with('0'));
    let frac_ok = frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) && !f.ends_with('0'));
    if !(int_ok && frac_ok) {
        return Err(format!("{s:?} is not a non-negative decimal in canonical form"));
    }
    Ok(())
}
