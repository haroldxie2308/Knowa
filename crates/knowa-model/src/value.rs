//! Typed claim values (DESIGN §4.3). JSON form: an object with exactly one
//! discriminator key plus its allowed modifier. Any other key is `invalid`.

use crate::error::Result;
use crate::id::Id;
use crate::term::Term;
use crate::time::When;

/// A decimal kept as its exact string form `-?[0-9]+(\.[0-9]+)?`.
/// Floats never enter canonical data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dec(String);

impl Dec {
    pub fn parse(s: &str) -> Result<Dec> {
        let _ = s;
        todo!()
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Lossy conversion for indexing/ranking only — never written back to claims.
    pub fn to_f64(&self) -> f64 {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Geo {
    /// [-90, 90]
    pub lat: Dec,
    /// [-180, 180]
    pub lon: Dec,
    /// metres
    pub alt: Option<Dec>,
    /// uncertainty radius, metres, ≥ 0
    pub r: Option<Dec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// ≤ 64 KiB UTF-8; `lang` is a BCP 47 tag (syntax-checked loosely: `[A-Za-z0-9-]+`).
    Text { text: String, lang: Option<String> },
    Term(Term),
    /// Any id kind; Blob ids may carry a fragment. Placeholders only inside drafts.
    Ref(Id),
    /// |n| ≤ 2^53 − 1
    Int(i64),
    Dec(Dec),
    Bool(bool),
    When(When),
    Geo(Geo),
    /// `unit` is a non-empty UCUM string (not validated against UCUM in v0).
    Qty { qty: Dec, unit: String },
}

pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_SAFE_INT: i64 = (1 << 53) - 1;

impl Value {
    pub fn from_json(v: &serde_json::Value) -> Result<Value> {
        let _ = v;
        todo!()
    }

    pub fn to_json(&self) -> serde_json::Value {
        todo!()
    }

    pub fn text(s: impl Into<String>) -> Value {
        Value::Text { text: s.into(), lang: None }
    }

    /// The id this value references, if it is a `Ref`.
    pub fn as_ref_id(&self) -> Option<&Id> {
        match self {
            Value::Ref(id) => Some(id),
            _ => None,
        }
    }
}

impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Value::from_json(&v).map_err(|e| serde::de::Error::custom(e.message))
    }
}
