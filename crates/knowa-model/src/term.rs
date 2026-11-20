//! Terms: `ns:name` (DESIGN §4.2). Used for predicates and enum-like values.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Term(String);

impl Term {
    /// ns `[a-z][a-z0-9-]*`, name `[A-Za-z][A-Za-z0-9_.-]*`, joined by exactly one `:`.
    pub fn parse(s: &str) -> Result<Term> {
        let invalid = |why: &str| Error::invalid(format!("invalid term {s:?}: {why}"));
        let (ns, name) = s.split_once(':').ok_or_else(|| invalid("expected ns:name"))?;
        let mut ns_bytes = ns.bytes();
        let ns_ok = ns_bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && ns_bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ns_ok {
            return Err(invalid("namespace must match [a-z][a-z0-9-]*"));
        }
        let mut name_bytes = name.bytes();
        let name_ok = name_bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
            && name_bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !name_ok {
            return Err(invalid("name must match [A-Za-z][A-Za-z0-9_.-]*"));
        }
        Ok(Term(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn ns(&self) -> &str {
        self.parts().0
    }

    pub fn name(&self) -> &str {
        self.parts().1
    }

    fn parts(&self) -> (&str, &str) {
        match self.0.split_once(':') {
            Some(parts) => parts,
            // Invariant: a `Term` is only built by `parse`, which requires the `:`.
            None => unreachable!("Term {:?} has no ':'", self.0),
        }
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
