//! Typed claim values (DESIGN §4.3). JSON form: an object with exactly one
//! discriminator key plus its allowed modifier. Any other key is `invalid`.

use std::cmp::Ordering;

use serde_json::{Map, Value as Json};

use crate::error::{Error, Result};
use crate::id::Id;
use crate::term::Term;
use crate::time::When;

/// A decimal kept as its exact string form `-?[0-9]+(\.[0-9]+)?`.
/// Floats never enter canonical data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dec(String);

impl Dec {
    pub fn parse(s: &str) -> Result<Dec> {
        let digits = s.strip_prefix('-').unwrap_or(s);
        let (int, frac) = match digits.split_once('.') {
            Some((int, frac)) => (int, Some(frac)),
            None => (digits, None),
        };
        let all_digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if !(all_digits(int) && frac.is_none_or(all_digits)) {
            return Err(Error::invalid(format!("invalid decimal {s:?}: expected -?[0-9]+(.[0-9]+)?")));
        }
        Ok(Dec(s.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Lossy conversion for indexing/ranking only — never written back to claims.
    pub fn to_f64(&self) -> f64 {
        match self.0.parse::<f64>() {
            Ok(f) => f,
            // Invariant: `parse` admits only `-?[0-9]+(.[0-9]+)?`, a subset of Rust's
            // float syntax (out-of-range magnitudes become ±inf, not errors).
            Err(e) => unreachable!("Dec {:?} is not float syntax: {e}", self.0),
        }
    }
}

/// Exact numeric order of two strings in `Dec` grammar (`-0` == `0`, `1.50` == `1.5`).
pub(crate) fn dec_cmp(a: &str, b: &str) -> Ordering {
    // (negative, integer digits without leading zeros, fraction without trailing zeros)
    fn split(s: &str) -> (bool, &str, &str) {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, s),
        };
        let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
        let (int, frac) = (int.trim_start_matches('0'), frac.trim_end_matches('0'));
        // zero has no sign
        (neg && !(int.is_empty() && frac.is_empty()), int, frac)
    }
    let (a_neg, a_int, a_frac) = split(a);
    let (b_neg, b_int, b_frac) = split(b);
    let magnitude = a_int.len().cmp(&b_int.len()).then_with(|| a_int.cmp(b_int)).then_with(|| a_frac.cmp(b_frac));
    match (a_neg, b_neg) {
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

/// `lo <= d <= hi`, exactly.
fn dec_within(d: &Dec, lo: &str, hi: &str) -> bool {
    dec_cmp(d.as_str(), lo) != Ordering::Less && dec_cmp(d.as_str(), hi) != Ordering::Greater
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

#[derive(Clone, Copy)]
enum Discriminator {
    Text,
    Term,
    Ref,
    Int,
    Dec,
    Bool,
    When,
    Geo,
    Qty,
}

const DISCRIMINATORS: [(&str, Discriminator); 9] = [
    ("text", Discriminator::Text),
    ("term", Discriminator::Term),
    ("ref", Discriminator::Ref),
    ("int", Discriminator::Int),
    ("dec", Discriminator::Dec),
    ("bool", Discriminator::Bool),
    ("when", Discriminator::When),
    ("geo", Discriminator::Geo),
    ("qty", Discriminator::Qty),
];

impl Value {
    pub fn from_json(v: &serde_json::Value) -> Result<Value> {
        let obj =
            v.as_object().ok_or_else(|| Error::invalid("value must be a JSON object such as {\"text\":\"…\"}"))?;
        let mut found: Option<(&str, Discriminator, &Json)> = None;
        for (key, field) in obj {
            if let Some(&(_, kind)) = DISCRIMINATORS.iter().find(|(name, _)| name == key) {
                if let Some((first, _, _)) = found {
                    return Err(Error::invalid(format!("value has two discriminators: `{first}` and `{key}`")));
                }
                found = Some((key, kind, field));
            }
        }
        let (discriminator, kind, field) = found.ok_or_else(|| {
            let names: Vec<&str> = DISCRIMINATORS.iter().map(|(name, _)| *name).collect();
            Error::invalid(format!("value has no discriminator (expected one of {})", names.join(", ")))
        })?;
        let modifier = match kind {
            Discriminator::Text => Some("lang"),
            Discriminator::Qty => Some("unit"),
            _ => None,
        };
        if let Some(key) = obj.keys().find(|k| *k != discriminator && Some(k.as_str()) != modifier) {
            return Err(Error::invalid(format!("unknown key `{key}` in a `{discriminator}` value")));
        }
        let value = match kind {
            Discriminator::Text => Value::Text {
                text: json_str(field, "text")?.to_string(),
                lang: obj.get("lang").map(|l| json_str(l, "lang").map(str::to_string)).transpose()?,
            },
            Discriminator::Term => Value::Term(Term::parse(json_str(field, "term")?)?),
            Discriminator::Ref => Value::Ref(Id::parse(json_str(field, "ref")?)?),
            Discriminator::Int => Value::Int(
                field.as_i64().ok_or_else(|| Error::invalid(format!("`int` must be an integer, got {field}")))?,
            ),
            Discriminator::Dec => Value::Dec(Dec::parse(json_str(field, "dec")?)?),
            Discriminator::Bool => Value::Bool(
                field.as_bool().ok_or_else(|| Error::invalid(format!("`bool` must be true or false, got {field}")))?,
            ),
            Discriminator::When => Value::When(When::parse(json_str(field, "when")?)?),
            Discriminator::Geo => Value::Geo(geo_from_json(field)?),
            Discriminator::Qty => Value::Qty {
                qty: Dec::parse(json_str(field, "qty")?)?,
                unit: json_str(obj.get("unit").ok_or_else(|| Error::invalid("`qty` value requires `unit`"))?, "unit")?
                    .to_string(),
            },
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut m = Map::new();
        match self {
            Value::Text { text, lang } => {
                m.insert("text".into(), Json::String(text.clone()));
                if let Some(lang) = lang {
                    m.insert("lang".into(), Json::String(lang.clone()));
                }
            }
            Value::Term(t) => {
                m.insert("term".into(), Json::String(t.as_str().into()));
            }
            Value::Ref(id) => {
                m.insert("ref".into(), Json::String(id.as_str().into()));
            }
            Value::Int(n) => {
                m.insert("int".into(), Json::from(*n));
            }
            Value::Dec(d) => {
                m.insert("dec".into(), Json::String(d.as_str().into()));
            }
            Value::Bool(b) => {
                m.insert("bool".into(), Json::Bool(*b));
            }
            Value::When(w) => {
                m.insert("when".into(), Json::String(w.raw.clone()));
            }
            Value::Geo(g) => {
                let mut geo = Map::new();
                geo.insert("lat".into(), Json::String(g.lat.as_str().into()));
                geo.insert("lon".into(), Json::String(g.lon.as_str().into()));
                if let Some(alt) = &g.alt {
                    geo.insert("alt".into(), Json::String(alt.as_str().into()));
                }
                if let Some(r) = &g.r {
                    geo.insert("r".into(), Json::String(r.as_str().into()));
                }
                m.insert("geo".into(), Json::Object(geo));
            }
            Value::Qty { qty, unit } => {
                m.insert("qty".into(), Json::String(qty.as_str().into()));
                m.insert("unit".into(), Json::String(unit.clone()));
            }
        }
        Json::Object(m)
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

    /// Rules the types alone do not enforce (the fields are public, so a value built in
    /// code may break them). Placeholder refs are allowed here; claims reject them.
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Value::Text { text, lang } => {
                if text.len() > MAX_TEXT_BYTES {
                    return Err(Error::invalid(format!(
                        "text is {} bytes; the limit is {MAX_TEXT_BYTES} (store larger content as a blob)",
                        text.len()
                    )));
                }
                if let Some(lang) = lang
                    && (lang.is_empty() || !lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                {
                    return Err(Error::invalid(format!("lang {lang:?} must match [A-Za-z0-9-]+")));
                }
            }
            Value::Int(n) => {
                if !(-MAX_SAFE_INT..=MAX_SAFE_INT).contains(n) {
                    return Err(Error::invalid(format!("int {n} is outside ±(2^53 − 1)")));
                }
            }
            Value::When(w) => w.check_consistent()?,
            Value::Geo(g) => {
                if !dec_within(&g.lat, "-90", "90") {
                    return Err(Error::invalid(format!("geo lat {} is outside [-90, 90]", g.lat.as_str())));
                }
                if !dec_within(&g.lon, "-180", "180") {
                    return Err(Error::invalid(format!("geo lon {} is outside [-180, 180]", g.lon.as_str())));
                }
                if let Some(r) = &g.r
                    && dec_cmp(r.as_str(), "0") == Ordering::Less
                {
                    return Err(Error::invalid(format!("geo r {} must be ≥ 0", r.as_str())));
                }
            }
            Value::Qty { unit, .. } => {
                if unit.is_empty() {
                    return Err(Error::invalid("qty unit must be non-empty"));
                }
            }
            Value::Term(_) | Value::Ref(_) | Value::Dec(_) | Value::Bool(_) => {}
        }
        Ok(())
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

fn json_str<'a>(v: &'a Json, key: &str) -> Result<&'a str> {
    v.as_str().ok_or_else(|| Error::invalid(format!("`{key}` must be a string, got {v}")))
}

fn geo_from_json(v: &Json) -> Result<Geo> {
    let obj = v.as_object().ok_or_else(|| Error::invalid("`geo` must be an object {\"lat\",\"lon\",\"alt\"?,\"r\"?}"))?;
    if let Some(key) = obj.keys().find(|k| !matches!(k.as_str(), "lat" | "lon" | "alt" | "r")) {
        return Err(Error::invalid(format!("unknown key `{key}` in `geo`")));
    }
    let dec = |key: &str| -> Result<Option<Dec>> {
        obj.get(key).map(|d| json_str(d, key).and_then(Dec::parse)).transpose()
    };
    let required = |key: &str| dec(key)?.ok_or_else(|| Error::invalid(format!("`geo` requires `{key}`")));
    Ok(Geo { lat: required("lat")?, lon: required("lon")?, alt: dec("alt")?, r: dec("r")? })
}

#[cfg(test)]
mod tests {
    use super::dec_cmp;
    use std::cmp::Ordering::*;

    #[test]
    fn dec_cmp_is_numeric() {
        for (a, b, want) in [
            ("1", "1.0", Equal),
            ("-0", "0.000", Equal),
            ("0.5", "0.49", Greater),
            ("0.5", "0.51", Less),
            ("10", "9.99", Greater),
            ("007", "7", Equal),
            ("-1", "0", Less),
            ("-2", "-10", Greater),
            ("-0.5", "-0.25", Less),
            ("90.0000001", "90", Greater),
        ] {
            assert_eq!(dec_cmp(a, b), want, "{a} vs {b}");
            assert_eq!(dec_cmp(b, a), want.reverse(), "{b} vs {a}");
        }
    }
}
