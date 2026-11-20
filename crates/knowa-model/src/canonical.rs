//! RFC 8785 (JCS) canonical JSON for the subset Knowa emits.

use serde_json::Value as Json;

use crate::error::{Error, Result};
use crate::value::MAX_SAFE_INT;

/// Serializes `v` per RFC 8785. Knowa restricts input so JCS is exact and simple:
/// - numbers must be integers with |n| ≤ 2^53 − 1 (floats → `invalid`);
/// - object keys must be ASCII (so UTF-16 code-unit order == byte order) → else `invalid`;
/// - strings: escape only `"`, `\`, and U+0000–U+001F (`\b \f \n \r \t` short forms,
///   others `\u00xx` lowercase); everything else, including U+2028/2029 and `/`, verbatim.
pub fn canonical_json(v: &serde_json::Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_value(v, &mut out)?;
    Ok(out)
}

fn write_value(v: &Json, out: &mut Vec<u8>) -> Result<()> {
    match v {
        Json::Null => out.extend_from_slice(b"null"),
        Json::Bool(true) => out.extend_from_slice(b"true"),
        Json::Bool(false) => out.extend_from_slice(b"false"),
        Json::Number(n) => {
            // `as_i64` is None for floats (even integral ones like 1.0) and for u64 > i64::MAX.
            let i = n.as_i64().filter(|i| (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(i)).ok_or_else(|| {
                Error::invalid(format!("canonical JSON allows only integers with |n| ≤ 2^53 − 1, got {n}"))
            })?;
            out.extend_from_slice(i.to_string().as_bytes());
        }
        Json::String(s) => write_string(s, out),
        Json::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Json::Object(map) => {
            if let Some(key) = map.keys().find(|k| !k.is_ascii()) {
                return Err(Error::invalid(format!("canonical JSON allows only ASCII object keys, got {key:?}")));
            }
            // Sorted here rather than trusting the map: serde_json's `preserve_order`
            // feature (enabled by any crate in the build) would make it insertion-ordered.
            let mut entries: Vec<(&String, &Json)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(value, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    // Byte-wise is exact: every byte of a multi-byte UTF-8 sequence is ≥ 0x80, so only
    // ASCII bytes can match the escapes below.
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1f => {
                out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xf)]]);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}
