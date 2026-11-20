//! Terms: `ns:name` (DESIGN §4.2). Used for predicates and enum-like values.

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Term(String);

impl Term {
    /// ns `[a-z][a-z0-9-]*`, name `[A-Za-z][A-Za-z0-9_.-]*`, joined by exactly one `:`.
    pub fn parse(s: &str) -> Result<Term> {
        let _ = s;
        todo!()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn ns(&self) -> &str {
        todo!()
    }

    pub fn name(&self) -> &str {
        todo!()
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for Term {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Term {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Term::parse(&s).map_err(|e| serde::de::Error::custom(e.message))
    }
}
